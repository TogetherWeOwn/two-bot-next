#![cfg(test)]
//! Starboard runtime acceptance: reaction-threshold highlights embeds.
//!
//! PROTECTED FILE (Test Author: TOG-36123). The paired Builder (TOG-36124)
//! builds on this branch and must never edit this file: every test below
//! must turn green through production code alone.
//!
//! Contract under test (design: TOG-35924 spec): gateway reaction events flow
//! through [`CommandRuntime::dispatch`], which gains a starboard arm for
//! `ReactionAdd` / `ReactionRemove` / `MessageDelete`. The arm loads per-guild
//! config (skip when the board channel is unset), applies the filter matrix
//! (self-star, bot author, excluded channel, NSFW source, non-configured
//! emoji), upserts/removes the starrer row, recounts distinct starrers from
//! the database, and posts-or-edits one board embed with the absolute count.
//!
//! Seams the tests assume (GREEN contract for the Builder):
//! - `CommandRuntime::new` wires starboard handling (leveling-style: always
//!   constructed from pool + executor, gated by DB config). No new
//!   constructor; the tests call `dispatch` on the existing runtime.
//! - Source message content and channel NSFW state arrive over the executor's
//!   REST path (`GET .../channels/{c}/messages/{m}`, `GET .../channels/{c}`);
//!   the route-aware mock below serves both, plus `/users/` and `/members/`
//!   fallbacks. A `MessageCreate` is dispatched first so a cache-backed
//!   design sees the same content.
//! - Sends/edits go through the existing 429-aware executor path.
//! - Schema: `starboard_config` / `starboard_starrers` / `starboard_posts`
//!   exactly as the TOG-35924 spec names them (PR1's migration creates them;
//!   the `IF NOT EXISTS` gap DDL below only fills the pre-PR1 window and
//!   never fires once PR1 lands). Seed/read SQL binds ids as text and casts
//!   columns (`guild_id::TEXT`, `star_count::BIGINT`), so it is agnostic to
//!   TEXT-vs-BIGINT column types.
//!
//! CI routing (the Builder adds this step to `check.yml`'s ignored-db-runtime
//! job; serial like the other runtime lanes):
//! `cargo test -p two-bot --locked starboard_ -- --include-ignored --test-threads=1`
//!
//! Fixture hygiene: ids, timestamps, avatar hashes, attachment URLs and
//! message text deliberately avoid the digits of asserted counts, so a count
//! substring in an embed can only be the rendered live count.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use twilight_gateway::Event;
use twilight_model::{
    channel::message::EmojiReactionType,
    channel::message::Message,
    channel::Channel,
    gateway::payload::incoming::{MessageCreate, MessageDelete, ReactionAdd, ReactionRemove},
    gateway::GatewayReaction,
    guild::Member,
    id::Id,
    user::User,
};
use two_bot_core::{InteractionRouter, RouterGates};
use two_bot_discord::ActionExecutor;

use crate::{
    command_runtime::CommandRuntime,
    discord_test_common::{MockRest, RestRequest, ScriptedResponse},
};

// ---------------------------------------------------------------------------
// Fixture ids (digit-hygienic: no asserted count substring occurs in any id,
// timestamp, hash, URL or message text used below).
// ---------------------------------------------------------------------------

const GUILD: u64 = 6666;
const SOURCE_CHANNEL: u64 = 7777;
const NSFW_CHANNEL: u64 = 7778;
const CONTROL_CHANNEL: u64 = 7779;
const BOARD_CHANNEL: u64 = 8888;
const AUTHOR: u64 = 6000;
const MESSAGE: u64 = 6060;
const LONG_MESSAGE: u64 = 6061;
const PLAIN_MESSAGE: u64 = 6062;
const MISSING_MESSAGE: u64 = 6063;
const BOARD_MESSAGE: u64 = 8080;

const AUTHOR_NAME: &str = "member";
const AVATAR_HASH: &str = "abcdefabcdefabcdefabcdefabcdefab";
const MESSAGE_TEXT: &str = "hello starboard world";
const STAMP: &str = "2026-09-28T00:00:00.000000+00:00";
const STAMP_DAY: &str = "2026-09-28";

fn long_text() -> String {
    "starboard probe alpha ".repeat(250)[..5000].to_owned()
}

// ---------------------------------------------------------------------------
// Discord content fixtures (served by the mock AND dispatched as
// MessageCreate, so fetch-backed and cache-backed designs see the same data).
// Each builder validates its JSON against the real twilight type, so a
// fixture/parse failure can never masquerade as a runtime failure.
// ---------------------------------------------------------------------------

fn attachments_for(message_id: u64) -> Value {
    json!([
        {
            "id": "7001",
            "filename": "notes.pdf",
            "size": 120,
            "url": format!("https://cdn.discordapp.com/attachments/7777/{message_id}/notes.pdf"),
            "proxy_url": format!("https://media.discordapp.net/attachments/7777/{message_id}/notes.pdf"),
            "content_type": "application/pdf",
        },
        {
            "id": "7002",
            "filename": "art.png",
            "size": 456,
            "url": format!("https://cdn.discordapp.com/attachments/7777/{message_id}/art.png"),
            "proxy_url": format!("https://media.discordapp.net/attachments/7777/{message_id}/art.png"),
            "content_type": "image/png",
            "width": 800,
            "height": 600,
        },
        {
            "id": "7003",
            "filename": "art2.png",
            "size": 789,
            "url": format!("https://cdn.discordapp.com/attachments/7777/{message_id}/art2.png"),
            "proxy_url": format!("https://media.discordapp.net/attachments/7777/{message_id}/art2.png"),
            "content_type": "image/png",
            "width": 400,
            "height": 300,
        },
    ])
}

fn message_json(
    message_id: u64,
    channel_id: u64,
    text: &str,
    attachments: Value,
    bot: bool,
) -> Value {
    let body = json!({
        "id": message_id.to_string(),
        "guild_id": GUILD.to_string(),
        "channel_id": channel_id.to_string(),
        "author": {
            "id": AUTHOR.to_string(),
            "username": AUTHOR_NAME,
            "discriminator": "0",
            "avatar": AVATAR_HASH,
            "bot": bot,
        },
        "content": text,
        "timestamp": STAMP,
        "edited_timestamp": null,
        "tts": false,
        "mention_everyone": false,
        "mentions": [],
        "mention_roles": [],
        "attachments": attachments,
        "embeds": [],
        "pinned": false,
        "type": 0,
        "components": [],
    });
    let _: Message = serde_json::from_value(body.clone()).expect("message fixture parses");
    body
}

fn channel_body(channel_id: u64, nsfw: bool) -> Value {
    let body = json!({
        "id": channel_id.to_string(),
        "guild_id": GUILD.to_string(),
        "type": 0,
        "name": "general",
        "nsfw": nsfw,
        "position": 0,
        "permission_overwrites": [],
        "rate_limit_per_user": 0,
        "parent_id": null,
        "topic": null,
    });
    let _: Channel = serde_json::from_value(body.clone()).expect("channel fixture parses");
    body
}

fn user_body() -> Value {
    let body = json!({
        "id": AUTHOR.to_string(),
        "username": AUTHOR_NAME,
        "discriminator": "0",
        "avatar": AVATAR_HASH,
        "bot": false,
    });
    let _: User = serde_json::from_value(body.clone()).expect("user fixture parses");
    body
}

fn member_body() -> Value {
    let body = json!({
        "user": user_body(),
        "roles": [],
        "deaf": false,
        "mute": false,
        "flags": 0,
        "joined_at": STAMP,
    });
    let _: Member = serde_json::from_value(body.clone()).expect("member fixture parses");
    body
}

/// Message content served for a source id, or `None` when the id is unknown
/// (the mock then answers 404, like Discord for an unknown message).
fn message_body(message_id: u64, config: &DoubleConfig) -> Option<Value> {
    match message_id {
        MESSAGE => Some(message_json(
            MESSAGE,
            config.message_channel,
            MESSAGE_TEXT,
            attachments_for(MESSAGE),
            config.bot_author,
        )),
        LONG_MESSAGE => Some(message_json(
            LONG_MESSAGE,
            config.message_channel,
            &long_text(),
            attachments_for(LONG_MESSAGE),
            config.bot_author,
        )),
        PLAIN_MESSAGE => Some(message_json(
            PLAIN_MESSAGE,
            CONTROL_CHANNEL,
            MESSAGE_TEXT,
            Value::Array(Vec::new()),
            false,
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Route-aware REST double: answers by method + path, never by position, so
// the tests accept any builder-internal request order.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct DoubleConfig {
    bot_author: bool,
    message_channel: u64,
    nsfw_channels: Vec<u64>,
    missing_messages: Vec<u64>,
}

impl Default for DoubleConfig {
    fn default() -> Self {
        Self {
            bot_author: false,
            message_channel: SOURCE_CHANNEL,
            nsfw_channels: Vec::new(),
            missing_messages: Vec::new(),
        }
    }
}

async fn start_double(config: DoubleConfig, fail_first_post: bool) -> MockRest {
    let config = Arc::new(config);
    let fail = Arc::new(AtomicBool::new(fail_first_post));
    MockRest::with_responder(move |req: &RestRequest| respond(req, &config, &fail)).await
}

fn respond(
    req: &RestRequest,
    config: &DoubleConfig,
    fail_first_post: &AtomicBool,
) -> ScriptedResponse {
    let ok = |body: Value| ScriptedResponse::json(200, body);
    let unknown =
        || ScriptedResponse::json(404, json!({"message": "Unknown Message", "code": 50035}));
    if req.method == "GET" && req.path.contains("/messages/") {
        let id = req
            .path
            .rsplit('/')
            .next()
            .and_then(|tail| tail.parse::<u64>().ok())
            .unwrap_or(0);
        if config.missing_messages.contains(&id) {
            return unknown();
        }
        if let Some(body) = message_body(id, config) {
            return ok(body);
        }
        return unknown();
    }
    if req.method == "GET" && req.path.contains("/channels/") {
        let id = req
            .path
            .rsplit('/')
            .next()
            .and_then(|tail| tail.parse::<u64>().ok())
            .unwrap_or(0);
        return ok(channel_body(id, config.nsfw_channels.contains(&id)));
    }
    if req.method == "GET" && req.path.contains("/members/") {
        return ok(member_body());
    }
    if req.method == "GET" && req.path.contains("/users/") {
        return ok(user_body());
    }
    if req.method == "POST" && req.path.ends_with("/messages") {
        if fail_first_post.swap(false, Ordering::SeqCst) {
            return ScriptedResponse::rate_limited(0.05, "1");
        }
        return ok(json!({"id": BOARD_MESSAGE.to_string()}));
    }
    if req.method == "PATCH" {
        return ok(json!({}));
    }
    ok(json!({}))
}

// ---------------------------------------------------------------------------
// Isolated test database: per-test schema over the cutover chain plus a
// pre-PR1 gap fill that no-ops once PR1's migration lands.
// ---------------------------------------------------------------------------

const STARBOARD_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS starboard_config (
  guild_id TEXT PRIMARY KEY,
  board_channel_id TEXT,
  threshold SMALLINT NOT NULL DEFAULT 3,
  emoji TEXT NOT NULL DEFAULT '⭐',
  excluded_channel_ids TEXT[] NOT NULL DEFAULT '{}',
  updated_by TEXT,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS starboard_starrers (
  guild_id TEXT NOT NULL,
  source_channel_id TEXT NOT NULL,
  source_message_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (guild_id, source_message_id, user_id)
);
CREATE TABLE IF NOT EXISTS starboard_posts (
  guild_id TEXT NOT NULL,
  source_channel_id TEXT NOT NULL,
  source_message_id TEXT NOT NULL,
  board_channel_id TEXT NOT NULL,
  board_message_id TEXT,
  star_count INTEGER NOT NULL DEFAULT 0,
  state TEXT NOT NULL DEFAULT 'posted',
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  CONSTRAINT starboard_posts_source_unique UNIQUE (guild_id, source_message_id)
);
CREATE INDEX IF NOT EXISTS idx_starboard_starrers_message
  ON starboard_starrers (guild_id, source_message_id);
"#;

const SEED_CONFIG: &str = "INSERT INTO starboard_config
  (guild_id, board_channel_id, threshold, emoji, excluded_channel_ids, updated_by, updated_at)
  VALUES ('6666', '8888', 3, '⭐', '{}', '77', now())";
const SEED_CONFIG_EXCLUDED: &str = "INSERT INTO starboard_config
  (guild_id, board_channel_id, threshold, emoji, excluded_channel_ids, updated_by, updated_at)
  VALUES ('6666', '8888', 3, '⭐', ARRAY['7777'], '77', now())";

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

struct TestDb {
    pool: PgPool,
    admin: PgPool,
    schema: String,
}

impl TestDb {
    async fn new() -> Self {
        let url = std::env::var("TWO_GATEWAY_TEST_DATABASE_URL")
            .expect("set the dedicated test URL; runtime DATABASE_URL is never used");
        let ci = std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
        let host = match url.as_str() {
            "postgres://agent_test:@agent-testdb:5432/agent_test" => "agent-testdb",
            "postgresql://agent_test@localhost:5432/agent_test" if ci => "localhost",
            _ => panic!("non-test database refused"),
        };
        assert!(std::env::var_os("PGOPTIONS").is_none());
        let options = PgConnectOptions::new_without_pgpass()
            .host(host)
            .port(5432)
            .username("agent_test")
            .password("")
            .database("agent_test")
            .ssl_mode(PgSslMode::Disable);
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .expect("agent test database");
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis();
        let schema = format!(
            "starboard_test_{}_{}_{}",
            std::process::id(),
            millis,
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed)
        );
        assert!(schema
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .expect("create isolated schema");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.options([("search_path", schema.clone())]))
            .await
            .expect("scoped test pool");
        sqlx::migrate!("../cutover/migrations")
            .run(&pool)
            .await
            .expect("migrations");
        // Both chains: PR1 lands its migration in the store chain for sure
        // and in the cutover chain for production; cross-running is additive
        // and safe in either order, so the tests find the tables wherever
        // they land. Pre-PR1 neither chain has them and the gap DDL fills in.
        two_bot_store::migrate(&pool)
            .await
            .expect("store migrations");
        sqlx::raw_sql(STARBOARD_DDL)
            .execute(&pool)
            .await
            .expect("starboard gap tables");
        Self {
            pool,
            admin,
            schema,
        }
    }

    async fn close(self) {
        self.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&self.admin)
        .await
        .expect("drop own test schema");
        self.admin.close().await;
    }

    async fn seed_config(&self) {
        sqlx::query(SEED_CONFIG)
            .execute(&self.pool)
            .await
            .expect("seed starboard config");
    }

    async fn seed_config_excluded(&self) {
        sqlx::query(SEED_CONFIG_EXCLUDED)
            .execute(&self.pool)
            .await
            .expect("seed starboard config with exclusion");
    }

    /// (board_message_id, star_count, state); casts keep the read agnostic to
    /// PR1's column types.
    async fn post_row(&self, message_id: u64) -> Option<(Option<String>, i64, String)> {
        sqlx::query_as(
            "SELECT board_message_id::TEXT, star_count::BIGINT, state::TEXT
               FROM starboard_posts
              WHERE guild_id::TEXT = $1 AND source_message_id::TEXT = $2",
        )
        .bind(GUILD.to_string())
        .bind(message_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .expect("post row")
    }

    async fn starrer_ids(&self, message_id: u64) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT user_id::TEXT FROM starboard_starrers
              WHERE guild_id::TEXT = $1 AND source_message_id::TEXT = $2 ORDER BY 1",
        )
        .bind(GUILD.to_string())
        .bind(message_id.to_string())
        .fetch_all(&self.pool)
        .await
        .expect("starrer rows")
    }

    /// Every starboard row as JSON text: the data-minimization tests assert the
    /// source message text appears nowhere in it.
    async fn dump_starboard(&self) -> String {
        let mut dump = String::new();
        for query in [
            "SELECT to_jsonb(t)::TEXT FROM starboard_config t",
            "SELECT to_jsonb(t)::TEXT FROM starboard_posts t",
            "SELECT to_jsonb(t)::TEXT FROM starboard_starrers t",
        ] {
            let rows: Vec<String> = sqlx::query_scalar(query)
                .fetch_all(&self.pool)
                .await
                .expect("starboard dump");
            for row in rows {
                dump.push_str(&row);
                dump.push('\n');
            }
        }
        dump
    }

    async fn audit_count(&self) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM automation_audit_log WHERE guild_id::TEXT = $1",
        )
        .bind(GUILD.to_string())
        .fetch_one(&self.pool)
        .await
        .expect("audit rows")
    }
}

// ---------------------------------------------------------------------------
// Runtime + gateway packets + wait helpers.
// ---------------------------------------------------------------------------

async fn runtime(pool: PgPool, mock: &MockRest) -> Arc<CommandRuntime> {
    let executor =
        ActionExecutor::with_proxy("starboard-test-token".to_owned(), Some(mock.origin()))
            .expect("mock executor");
    let router = InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD),
        scorecard: false,
        automations: true,
        announcements: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    });
    CommandRuntime::new(pool, executor, router, GUILD, true)
}

fn reaction(user_id: u64, message_id: u64, channel_id: u64, emoji: &str) -> GatewayReaction {
    GatewayReaction {
        burst: false,
        burst_colors: Vec::new(),
        channel_id: Id::new(channel_id),
        emoji: EmojiReactionType::Unicode {
            name: emoji.to_owned(),
        },
        guild_id: Some(Id::new(GUILD)),
        member: None,
        message_author_id: Some(Id::new(AUTHOR)),
        message_id: Id::new(message_id),
        user_id: Id::new(user_id),
    }
}

fn add_event(user_id: u64, message_id: u64, channel_id: u64, emoji: &str) -> Event {
    Event::ReactionAdd(Box::new(ReactionAdd(reaction(
        user_id, message_id, channel_id, emoji,
    ))))
}

fn remove_event(user_id: u64, message_id: u64, channel_id: u64) -> Event {
    Event::ReactionRemove(Box::new(ReactionRemove(reaction(
        user_id, message_id, channel_id, "⭐",
    ))))
}

fn created_event(message_id: u64, channel_id: u64, bot: bool) -> Event {
    let (text, attachments) = match message_id {
        LONG_MESSAGE => (long_text(), attachments_for(LONG_MESSAGE)),
        PLAIN_MESSAGE => (MESSAGE_TEXT.to_owned(), Value::Array(Vec::new())),
        _ => (MESSAGE_TEXT.to_owned(), attachments_for(message_id)),
    };
    let created: MessageCreate = serde_json::from_value(message_json(
        message_id,
        channel_id,
        &text,
        attachments,
        bot,
    ))
    .expect("message create fixture");
    Event::MessageCreate(Box::new(created))
}

fn delete_event(message_id: u64, channel_id: u64) -> Event {
    Event::MessageDelete(MessageDelete {
        channel_id: Id::new(channel_id),
        guild_id: Some(Id::new(GUILD)),
        id: Id::new(message_id),
    })
}

async fn wait_for(predicate: impl Fn() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if predicate() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Wait until no new REST traffic arrives for 600 ms (cap 20 s).
async fn settle(mock: &MockRest) {
    let start = tokio::time::Instant::now();
    let mut observed = mock.requests().len();
    let mut quiet_since = start;
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = tokio::time::Instant::now();
        let current = mock.requests().len();
        if current != observed {
            observed = current;
            quiet_since = now;
        }
        if now.duration_since(quiet_since) > Duration::from_millis(600) {
            return;
        }
        assert!(
            now.duration_since(start) < Duration::from_secs(20),
            "mock traffic never settled"
        );
    }
}

fn board_posts(requests: &[RestRequest]) -> Vec<RestRequest> {
    let suffix = format!("/channels/{BOARD_CHANNEL}/messages");
    requests
        .iter()
        .filter(|req| req.method == "POST" && req.path.ends_with(&suffix))
        .cloned()
        .collect()
}

fn board_patches(requests: &[RestRequest]) -> Vec<RestRequest> {
    let needle = format!("/channels/{BOARD_CHANNEL}/messages/");
    requests
        .iter()
        .filter(|req| req.method == "PATCH" && req.path.contains(&needle))
        .cloned()
        .collect()
}

fn message_writes(requests: &[RestRequest]) -> Vec<RestRequest> {
    requests
        .iter()
        .filter(|req| {
            matches!(req.method.as_str(), "POST" | "PATCH" | "PUT" | "DELETE")
                && req.path.contains("/messages")
        })
        .cloned()
        .collect()
}

fn deletes(requests: &[RestRequest]) -> Vec<RestRequest> {
    requests
        .iter()
        .filter(|req| req.method == "DELETE")
        .cloned()
        .collect()
}

fn body_json(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("request body is JSON")
}

/// All embed text surfaces joined: title, description, author name, footer,
/// field names and values. The live count must render inside the embed.
fn embed_text(embed: &Value) -> String {
    let mut parts = [
        embed["title"].as_str().unwrap_or(""),
        embed["description"].as_str().unwrap_or(""),
        embed["author"]["name"].as_str().unwrap_or(""),
        embed["footer"]["text"].as_str().unwrap_or(""),
    ]
    .join("\n");
    if let Some(fields) = embed["fields"].as_array() {
        for field in fields {
            parts.push_str(field["name"].as_str().unwrap_or(""));
            parts.push('\n');
            parts.push_str(field["value"].as_str().unwrap_or(""));
            parts.push('\n');
        }
    }
    parts
}

fn jump_link(message_id: u64, channel_id: u64) -> String {
    format!("https://discord.com/channels/{GUILD}/{channel_id}/{message_id}")
}

// ---------------------------------------------------------------------------
// Post / edit lifecycle.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_add_reaches_threshold_posts_board_embed() {
    let db = TestDb::new().await;
    let mock = start_double(DoubleConfig::default(), false).await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    runtime.dispatch(&created_event(MESSAGE, SOURCE_CHANNEL, false));
    for user_id in [6001, 6002] {
        runtime.dispatch(&add_event(user_id, MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        board_posts(&mock.requests()).is_empty(),
        "no board post below threshold"
    );
    runtime.dispatch(&add_event(6003, MESSAGE, SOURCE_CHANNEL, "⭐"));
    wait_for(
        || !board_posts(&mock.requests()).is_empty(),
        "board post at threshold",
    )
    .await;
    settle(&mock).await;

    let posts = board_posts(&mock.requests());
    assert_eq!(posts.len(), 1, "exactly one board post");
    let body = body_json(&posts[0].body);
    let embed = &body["embeds"][0];
    assert_eq!(embed["author"]["name"], AUTHOR_NAME);
    assert!(
        embed["author"]["icon_url"]
            .as_str()
            .unwrap_or("")
            .contains(AVATAR_HASH),
        "avatar hash in author icon"
    );
    assert_eq!(embed["description"], MESSAGE_TEXT);
    assert!(
        embed["timestamp"]
            .as_str()
            .unwrap_or("")
            .contains(STAMP_DAY),
        "source timestamp on the embed"
    );
    assert_eq!(
        embed["image"]["url"], "https://cdn.discordapp.com/attachments/7777/6060/art.png",
        "first image attachment wins over the leading pdf"
    );
    assert!(
        serde_json::to_string(embed)
            .expect("embed serializes")
            .contains(&jump_link(MESSAGE, SOURCE_CHANNEL)),
        "jump-to-message link inside the embed"
    );
    assert!(
        embed_text(embed).contains('3'),
        "live count renders inside the embed"
    );
    let row = db.post_row(MESSAGE).await.expect("post row recorded");
    assert_eq!(row.0, Some(BOARD_MESSAGE.to_string()));
    assert_eq!(row.1, 3);
    assert_eq!(row.2, "posted");
    assert!(deletes(&mock.requests()).is_empty());
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_add_above_threshold_edits_count() {
    let db = TestDb::new().await;
    let mock = start_double(DoubleConfig::default(), false).await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    runtime.dispatch(&created_event(MESSAGE, SOURCE_CHANNEL, false));
    for user_id in [6001, 6002, 6003] {
        runtime.dispatch(&add_event(user_id, MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    wait_for(
        || !board_posts(&mock.requests()).is_empty(),
        "board post at threshold",
    )
    .await;
    settle(&mock).await;
    assert_eq!(board_posts(&mock.requests()).len(), 1);

    runtime.dispatch(&add_event(6004, MESSAGE, SOURCE_CHANNEL, "⭐"));
    wait_for(
        || !board_patches(&mock.requests()).is_empty(),
        "count edit above threshold",
    )
    .await;
    settle(&mock).await;
    assert_eq!(
        board_posts(&mock.requests()).len(),
        1,
        "later count changes are edits, never reposts"
    );
    let patches = board_patches(&mock.requests());
    assert_eq!(
        patches[0].path,
        format!("/api/v10/channels/{BOARD_CHANNEL}/messages/{BOARD_MESSAGE}"),
        "edit targets the recorded board message"
    );
    let embed = &body_json(&patches[0].body)["embeds"][0];
    assert!(
        embed_text(embed).contains('4'),
        "edit carries the absolute recounted total"
    );
    let row = db.post_row(MESSAGE).await.expect("post row recorded");
    assert_eq!(row.1, 4);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_remove_edits_down_and_keeps_below_threshold_post() {
    let db = TestDb::new().await;
    let mock = start_double(DoubleConfig::default(), false).await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    runtime.dispatch(&created_event(MESSAGE, SOURCE_CHANNEL, false));
    for user_id in [6001, 6002, 6003, 6004] {
        runtime.dispatch(&add_event(user_id, MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    wait_for(
        || !board_posts(&mock.requests()).is_empty(),
        "board post at threshold",
    )
    .await;
    settle(&mock).await;
    assert_eq!(db.post_row(MESSAGE).await.expect("post row").1, 4);

    runtime.dispatch(&remove_event(6004, MESSAGE, SOURCE_CHANNEL));
    wait_for(
        || !board_patches(&mock.requests()).is_empty(),
        "count edit after remove",
    )
    .await;
    settle(&mock).await;
    let last = board_patches(&mock.requests())
        .pop()
        .expect("edit recorded");
    assert!(
        embed_text(&body_json(&last.body)["embeds"][0]).contains('3'),
        "remove edits the count down to the absolute total"
    );

    let patches_before = board_patches(&mock.requests()).len();
    runtime.dispatch(&remove_event(6003, MESSAGE, SOURCE_CHANNEL));
    wait_for(
        || board_patches(&mock.requests()).len() > patches_before,
        "below-threshold edit",
    )
    .await;
    settle(&mock).await;
    let row = db.post_row(MESSAGE).await.expect("post row kept");
    assert_eq!(row.1, 2);
    assert!(
        deletes(&mock.requests()).is_empty(),
        "below-threshold post is edited and kept, never deleted"
    );
    let last = board_patches(&mock.requests())
        .pop()
        .expect("edit recorded");
    let text = embed_text(&body_json(&last.body)["embeds"][0]);
    assert!(
        !text.contains('3') && !text.contains('4'),
        "below-threshold edit drops the stale count"
    );

    runtime.dispatch(&add_event(6003, MESSAGE, SOURCE_CHANNEL, "⭐"));
    wait_for(
        || {
            board_patches(&mock.requests())
                .iter()
                .any(|req| embed_text(&body_json(&req.body)["embeds"][0]).contains('3'))
        },
        "re-add edits the kept post back up",
    )
    .await;
    settle(&mock).await;
    assert_eq!(
        board_posts(&mock.requests()).len(),
        1,
        "the kept post is reused, never reposted"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_duplicate_delivery_writes_nothing() {
    let db = TestDb::new().await;
    let mock = start_double(DoubleConfig::default(), false).await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    runtime.dispatch(&created_event(MESSAGE, SOURCE_CHANNEL, false));
    for user_id in [6001, 6002, 6003] {
        runtime.dispatch(&add_event(user_id, MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    wait_for(
        || !board_posts(&mock.requests()).is_empty(),
        "board post at threshold",
    )
    .await;
    settle(&mock).await;
    let writes_before = message_writes(&mock.requests()).len();

    runtime.dispatch(&add_event(6001, MESSAGE, SOURCE_CHANNEL, "⭐"));
    runtime.dispatch(&add_event(6001, MESSAGE, SOURCE_CHANNEL, "⭐"));
    settle(&mock).await;
    assert_eq!(
        message_writes(&mock.requests()).len(),
        writes_before,
        "duplicate delivery is a no-op: no repost, no edit"
    );
    assert_eq!(db.post_row(MESSAGE).await.expect("post row").1, 3);
    assert_eq!(db.starrer_ids(MESSAGE).await.len(), 3);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_storm_coalesces_to_one_post_and_bounded_edits() {
    let db = TestDb::new().await;
    let mock = start_double(DoubleConfig::default(), false).await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    runtime.dispatch(&created_event(PLAIN_MESSAGE, CONTROL_CHANNEL, false));
    for user_id in [6001, 6002, 6003, 6004, 6005, 6006, 6007, 6008, 6009, 6011] {
        runtime.dispatch(&add_event(user_id, PLAIN_MESSAGE, CONTROL_CHANNEL, "⭐"));
    }
    wait_for(|| !board_posts(&mock.requests()).is_empty(), "storm post").await;
    settle(&mock).await;

    let posts = board_posts(&mock.requests());
    let patches = board_patches(&mock.requests());
    assert_eq!(posts.len(), 1, "exactly-once: one board post per source");
    assert!(
        patches.len() <= 3,
        "storm coalesces to bounded edits, saw {}",
        patches.len()
    );
    assert!(
        posts.len() + patches.len() <= 4,
        "a 10-reaction burst stays a post plus a handful of edits"
    );
    let last = patches
        .last()
        .map(|req| body_json(&req.body))
        .unwrap_or_else(|| body_json(&posts[0].body));
    assert!(
        embed_text(&last["embeds"][0]).contains("10"),
        "latest count wins"
    );
    assert_eq!(db.post_row(PLAIN_MESSAGE).await.expect("post row").1, 10);
    assert!(
        body_json(&posts[0].body)["embeds"][0]
            .get("image")
            .is_none_or(Value::is_null),
        "messages without images post no image"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_source_delete_marks_post_and_drops_jump_link() {
    let db = TestDb::new().await;
    let mock = start_double(DoubleConfig::default(), false).await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    runtime.dispatch(&created_event(MESSAGE, SOURCE_CHANNEL, false));
    for user_id in [6001, 6002, 6003] {
        runtime.dispatch(&add_event(user_id, MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    wait_for(
        || !board_posts(&mock.requests()).is_empty(),
        "board post at threshold",
    )
    .await;
    settle(&mock).await;

    runtime.dispatch(&delete_event(MESSAGE, SOURCE_CHANNEL));
    wait_for(
        || !board_patches(&mock.requests()).is_empty(),
        "source-deleted edit",
    )
    .await;
    settle(&mock).await;
    let patches = board_patches(&mock.requests());
    assert_eq!(patches.len(), 1, "source delete edits the post once");
    let raw = serde_json::to_string(&body_json(&patches[0].body)).expect("edit serializes");
    assert!(
        raw.contains(AUTHOR_NAME),
        "attribution survives the source delete"
    );
    assert!(
        !raw.contains("discord.com/channels"),
        "jump link is dropped with the source"
    );
    let row = db.post_row(MESSAGE).await.expect("post row kept");
    assert_eq!(row.2, "source_deleted");
    assert_eq!(row.0, Some(BOARD_MESSAGE.to_string()));
    assert!(deletes(&mock.requests()).is_empty());
    mock.shutdown().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// Filter matrix.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_self_star_excluded_from_count() {
    let db = TestDb::new().await;
    let mock = start_double(DoubleConfig::default(), false).await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    runtime.dispatch(&created_event(MESSAGE, SOURCE_CHANNEL, false));
    runtime.dispatch(&add_event(AUTHOR, MESSAGE, SOURCE_CHANNEL, "⭐"));
    for user_id in [6001, 6002, 6003] {
        runtime.dispatch(&add_event(user_id, MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    wait_for(
        || !board_posts(&mock.requests()).is_empty(),
        "board post from countable stars",
    )
    .await;
    settle(&mock).await;

    let posts = board_posts(&mock.requests());
    assert_eq!(posts.len(), 1);
    assert!(
        embed_text(&body_json(&posts[0].body)["embeds"][0]).contains('3'),
        "the author's own star never counts"
    );
    assert_eq!(
        db.starrer_ids(MESSAGE).await,
        vec!["6001".to_owned(), "6002".to_owned(), "6003".to_owned()]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_bot_author_skipped() {
    let db = TestDb::new().await;
    let mock = start_double(
        DoubleConfig {
            bot_author: true,
            ..DoubleConfig::default()
        },
        false,
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    runtime.dispatch(&created_event(MESSAGE, SOURCE_CHANNEL, true));
    for user_id in [6001, 6002, 6003] {
        runtime.dispatch(&add_event(user_id, MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        message_writes(&mock.requests()).is_empty(),
        "bot-authored sources are never reposted"
    );

    // Liveness control on the same harness: a human-authored message posts.
    runtime.dispatch(&created_event(PLAIN_MESSAGE, CONTROL_CHANNEL, false));
    for user_id in [6001, 6002, 6003] {
        runtime.dispatch(&add_event(user_id, PLAIN_MESSAGE, CONTROL_CHANNEL, "⭐"));
    }
    wait_for(
        || !board_posts(&mock.requests()).is_empty(),
        "control post proves the pipeline is live",
    )
    .await;
    settle(&mock).await;
    let posts = board_posts(&mock.requests());
    assert_eq!(posts.len(), 1);
    assert!(
        serde_json::to_string(&body_json(&posts[0].body))
            .expect("post serializes")
            .contains(&jump_link(PLAIN_MESSAGE, CONTROL_CHANNEL)),
        "only the control message reached the board"
    );
    assert!(db.starrer_ids(MESSAGE).await.is_empty());
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_excluded_channel_skipped() {
    let db = TestDb::new().await;
    let mock = start_double(DoubleConfig::default(), false).await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config_excluded().await;
    runtime.dispatch(&created_event(MESSAGE, SOURCE_CHANNEL, false));
    for user_id in [6001, 6002, 6003] {
        runtime.dispatch(&add_event(user_id, MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        message_writes(&mock.requests()).is_empty(),
        "excluded channels are never reposted"
    );

    runtime.dispatch(&created_event(PLAIN_MESSAGE, CONTROL_CHANNEL, false));
    for user_id in [6001, 6002, 6003] {
        runtime.dispatch(&add_event(user_id, PLAIN_MESSAGE, CONTROL_CHANNEL, "⭐"));
    }
    wait_for(
        || !board_posts(&mock.requests()).is_empty(),
        "control post proves the pipeline is live",
    )
    .await;
    settle(&mock).await;
    assert_eq!(board_posts(&mock.requests()).len(), 1);
    assert!(db.starrer_ids(MESSAGE).await.is_empty());
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_nsfw_source_skipped() {
    let db = TestDb::new().await;
    let mock = start_double(
        DoubleConfig {
            message_channel: NSFW_CHANNEL,
            nsfw_channels: vec![NSFW_CHANNEL],
            ..DoubleConfig::default()
        },
        false,
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    runtime.dispatch(&created_event(MESSAGE, NSFW_CHANNEL, false));
    for user_id in [6001, 6002, 6003] {
        runtime.dispatch(&add_event(user_id, MESSAGE, NSFW_CHANNEL, "⭐"));
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        message_writes(&mock.requests()).is_empty(),
        "NSFW sources never cross into the highlights feed"
    );

    runtime.dispatch(&created_event(PLAIN_MESSAGE, CONTROL_CHANNEL, false));
    for user_id in [6001, 6002, 6003] {
        runtime.dispatch(&add_event(user_id, PLAIN_MESSAGE, CONTROL_CHANNEL, "⭐"));
    }
    wait_for(
        || !board_posts(&mock.requests()).is_empty(),
        "control post proves the pipeline is live",
    )
    .await;
    settle(&mock).await;
    assert_eq!(board_posts(&mock.requests()).len(), 1);
    assert!(db.starrer_ids(MESSAGE).await.is_empty());
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_other_emoji_not_counted() {
    let db = TestDb::new().await;
    let mock = start_double(DoubleConfig::default(), false).await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    runtime.dispatch(&created_event(MESSAGE, SOURCE_CHANNEL, false));
    for user_id in [6001, 6002] {
        runtime.dispatch(&add_event(user_id, MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    for user_id in [6003, 6004, 6005] {
        runtime.dispatch(&add_event(user_id, MESSAGE, SOURCE_CHANNEL, "👍"));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        board_posts(&mock.requests()).is_empty(),
        "only the configured emoji counts toward threshold"
    );
    runtime.dispatch(&add_event(6003, MESSAGE, SOURCE_CHANNEL, "⭐"));
    wait_for(
        || !board_posts(&mock.requests()).is_empty(),
        "board post at three countable stars",
    )
    .await;
    settle(&mock).await;

    let posts = board_posts(&mock.requests());
    assert_eq!(posts.len(), 1);
    assert!(
        embed_text(&body_json(&posts[0].body)["embeds"][0]).contains('3'),
        "non-configured reactions never inflate the count"
    );
    assert_eq!(
        db.starrer_ids(MESSAGE).await,
        vec!["6001".to_owned(), "6002".to_owned(), "6003".to_owned()]
    );
    mock.shutdown().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// Config gating, retries, minimization, audit silence, truncation, fallback.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_unconfigured_board_skips_before_upsert() {
    let db = TestDb::new().await;
    let mock = start_double(DoubleConfig::default(), false).await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    runtime.dispatch(&created_event(MESSAGE, SOURCE_CHANNEL, false));
    for user_id in [6001, 6002, 6003] {
        runtime.dispatch(&add_event(user_id, MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        message_writes(&mock.requests()).is_empty(),
        "the feature is off until a board channel is configured"
    );

    // Configuring later must not resurrect the skipped stars: only newly
    // observed reactions count.
    db.seed_config().await;
    runtime.dispatch(&add_event(6004, MESSAGE, SOURCE_CHANNEL, "⭐"));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        board_posts(&mock.requests()).is_empty(),
        "pre-configuration stars left no rows behind"
    );
    for user_id in [6001, 6002] {
        runtime.dispatch(&add_event(user_id, MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    wait_for(
        || !board_posts(&mock.requests()).is_empty(),
        "board post from post-configuration stars",
    )
    .await;
    settle(&mock).await;
    let posts = board_posts(&mock.requests());
    assert_eq!(posts.len(), 1);
    assert!(
        embed_text(&body_json(&posts[0].body)["embeds"][0]).contains('3'),
        "exactly the three post-configuration stars count"
    );
    assert_eq!(
        db.starrer_ids(MESSAGE).await,
        vec!["6001".to_owned(), "6002".to_owned(), "6004".to_owned()],
        "the pre-configuration star from 6003 was never recorded"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_429_retries_to_single_post_with_absolute_count() {
    let db = TestDb::new().await;
    let mock = start_double(DoubleConfig::default(), true).await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    runtime.dispatch(&created_event(MESSAGE, SOURCE_CHANNEL, false));
    for user_id in [6001, 6002, 6003] {
        runtime.dispatch(&add_event(user_id, MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    wait_for(
        || board_posts(&mock.requests()).len() >= 2,
        "rate-limited post is retried",
    )
    .await;
    settle(&mock).await;

    let posts = board_posts(&mock.requests());
    assert!(
        (2..=4).contains(&posts.len()),
        "bounded retries after a 429, saw {}",
        posts.len()
    );
    assert!(
        board_patches(&mock.requests()).is_empty(),
        "no spurious edit around the retry"
    );
    let row = db.post_row(MESSAGE).await.expect("post row recorded");
    assert_eq!(
        row.0,
        Some(BOARD_MESSAGE.to_string()),
        "exactly one logical board post across the retry"
    );
    assert_eq!(row.1, 3, "retry re-syncs the absolute count");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_stores_ids_not_message_text() {
    let db = TestDb::new().await;
    let mock = start_double(DoubleConfig::default(), false).await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    runtime.dispatch(&created_event(LONG_MESSAGE, SOURCE_CHANNEL, false));
    for user_id in [6001, 6002, 6003] {
        runtime.dispatch(&add_event(user_id, LONG_MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    wait_for(
        || !board_posts(&mock.requests()).is_empty(),
        "board post at threshold",
    )
    .await;
    settle(&mock).await;

    let posts = board_posts(&mock.requests());
    assert_eq!(posts.len(), 1);
    assert!(
        serde_json::to_string(&body_json(&posts[0].body))
            .expect("post serializes")
            .contains("probe alpha"),
        "the embed carries the text (so its absence below is meaningful)"
    );
    assert!(
        !db.dump_starboard().await.contains("probe alpha"),
        "ids, counts and state are stored; message text never is"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
// Named without "audit_": patch-coverage's ignored sweep skips that
// substring and this test must run there until the builder's lane lands.
async fn starboard_per_star_flow_adds_no_log_rows() {
    let db = TestDb::new().await;
    let mock = start_double(DoubleConfig::default(), false).await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    let audits_before = db.audit_count().await;
    runtime.dispatch(&created_event(MESSAGE, SOURCE_CHANNEL, false));
    for user_id in [6001, 6002, 6003] {
        runtime.dispatch(&add_event(user_id, MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    wait_for(
        || !board_posts(&mock.requests()).is_empty(),
        "board post at threshold",
    )
    .await;
    settle(&mock).await;
    assert_eq!(
        db.audit_count().await,
        audits_before,
        "per-star reposts stay out of the audit log (config changes audit, stars do not)"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_embed_truncates_long_text() {
    let db = TestDb::new().await;
    let mock = start_double(DoubleConfig::default(), false).await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    runtime.dispatch(&created_event(LONG_MESSAGE, SOURCE_CHANNEL, false));
    for user_id in [6001, 6002, 6003] {
        runtime.dispatch(&add_event(user_id, LONG_MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    wait_for(
        || !board_posts(&mock.requests()).is_empty(),
        "board post at threshold",
    )
    .await;
    settle(&mock).await;

    let posts = board_posts(&mock.requests());
    assert_eq!(posts.len(), 1);
    let description = body_json(&posts[0].body)["embeds"][0]["description"]
        .as_str()
        .expect("embed description")
        .to_owned();
    assert!(description.len() < 5000, "overlong text is truncated");
    assert!(
        description.len() <= 4096,
        "truncation respects the embed description limit"
    );
    assert!(
        description.starts_with("starboard probe alpha"),
        "truncation keeps the head of the message"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_message_fetch_failure_posts_degraded_embed() {
    let db = TestDb::new().await;
    let mock = start_double(
        DoubleConfig {
            missing_messages: vec![MISSING_MESSAGE],
            ..DoubleConfig::default()
        },
        false,
    )
    .await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    // No MessageCreate: neither a fetch-backed nor a cache-backed design can
    // see this message's content.
    for user_id in [6001, 6002, 6003] {
        runtime.dispatch(&add_event(user_id, MISSING_MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    wait_for(
        || !board_posts(&mock.requests()).is_empty(),
        "degraded board post despite fetch failure",
    )
    .await;
    settle(&mock).await;

    let posts = board_posts(&mock.requests());
    assert_eq!(posts.len(), 1);
    let raw = serde_json::to_string(&body_json(&posts[0].body)).expect("post serializes");
    assert!(
        raw.contains(&jump_link(MISSING_MESSAGE, SOURCE_CHANNEL)),
        "degraded embed keeps the jump link"
    );
    assert!(
        raw.contains(&AUTHOR.to_string()),
        "degraded embed keeps the author reference"
    );
    mock.shutdown().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// Controls: green before and after the build; they prove the harness and
// guard behavior the implementation must preserve.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_remove_unknown_starrer_is_silent() {
    let db = TestDb::new().await;
    let mock = start_double(DoubleConfig::default(), false).await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    runtime.dispatch(&created_event(MESSAGE, SOURCE_CHANNEL, false));
    runtime.dispatch(&remove_event(6001, MESSAGE, SOURCE_CHANNEL));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        message_writes(&mock.requests()).is_empty(),
        "removing a never-recorded star changes nothing"
    );
    assert!(db.post_row(MESSAGE).await.is_none());
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn starboard_per_star_reposts_stay_off_info_logs() {
    use crate::tracing_capture;

    let db = TestDb::new().await;
    let mock = start_double(DoubleConfig::default(), false).await;
    let runtime = runtime(db.pool.clone(), &mock).await;
    db.seed_config().await;
    let capture = tracing_capture::global();
    let start = capture.text().len();
    runtime.dispatch(&created_event(MESSAGE, SOURCE_CHANNEL, false));
    for user_id in [6001, 6002, 6003, 6004, 6005] {
        runtime.dispatch(&add_event(user_id, MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    runtime.dispatch(&remove_event(6005, MESSAGE, SOURCE_CHANNEL));
    for user_id in [6006, 6007] {
        runtime.dispatch(&add_event(user_id, MESSAGE, SOURCE_CHANNEL, "⭐"));
    }
    wait_for(
        || !board_posts(&mock.requests()).is_empty(),
        "board post at threshold",
    )
    .await;
    settle(&mock).await;

    let fresh = capture.text()[start..].to_owned();
    let loud: Vec<&str> = fresh
        .lines()
        .filter(|line| {
            (line.starts_with("INFO ") || line.starts_with("WARN ") || line.starts_with("ERROR "))
                && (line.contains("starboard")
                    || line.contains(&MESSAGE.to_string())
                    || line.contains(&BOARD_MESSAGE.to_string()))
        })
        .collect();
    assert!(
        loud.len() <= 2,
        "eight star events must not log per-star at INFO or higher, saw {}: {loud:?}",
        loud.len()
    );
    mock.shutdown().await;
    db.close().await;
}
