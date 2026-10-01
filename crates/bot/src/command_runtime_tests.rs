//! Acceptance tests for the shared command runtime (TOG-11020; the sticky
//! slice's tests arrived with TOG-10309).
//!
//! Two layers, mirroring `gateway_tests.rs` + `interaction_routing.rs`:
//!
//! 1. In-memory units (always run): option extraction, reply shape, actor
//!    resolution, gate short-circuits, router-refusal answers, the detached
//!    `dispatch` path, and READY registry publication — all through a compact
//!    mock REST double.
//! 2. `#[ignore]` agent-testdb acceptance: burst coalescing to one re-post,
//!    previous-sticky retirement, `/sticky-remove` clearing state + message,
//!    claim collision, debounce hold, REST-failure cleanup, and the feed
//!    slice's CRUD + `announcements_audit_log` journeys. These run in CI via
//!    `TWO_GATEWAY_TEST_DATABASE_URL` (same approved service as the gateway
//!    suite — never the runtime DATABASE_URL).

use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use twilight_model::application::command::CommandType;
use twilight_model::application::interaction::application_command::{
    CommandData, CommandDataOption, CommandOptionValue,
};
use twilight_model::application::interaction::{Interaction, InteractionData, InteractionType};
use twilight_model::channel::message::{Message, MessageFlags, MessageType};
use twilight_model::gateway::event::Event;
use twilight_model::gateway::payload::incoming::{InteractionCreate, Ready};
use twilight_model::guild::{MemberFlags, Permissions, UnavailableGuild};
use twilight_model::http::interaction::InteractionResponseType;
use twilight_model::id::{AnonymizableId, Id};
use twilight_model::oauth::{ApplicationFlags, ApplicationIntegrationMap, PartialApplication};
use twilight_model::user::{CurrentUser, User};
use twilight_model::util::Timestamp;
use two_bot_core::funnel::now_millis_for_test;
use two_bot_core::sticky::{store, ActivityOutcome};
use two_bot_core::{
    RouterGates, RouterRefusal, ANNOUNCEMENTS_DISABLED_REPLY, AUTOMATIONS_DISABLED_REPLY,
    MANAGE_SERVER_REQUIRED,
};
use two_bot_discord::ActionExecutor;

use crate::command_runtime::{
    actor_id, ephemeral, feed_add_options, feed_remove_option, new_id, router_with_commands,
    sticky_options, CommandRuntime,
};

const GUILD: u64 = 2222;
const GUILD_S: &str = "2222";
const CHANNEL: u64 = 3333;
const CHANNEL_S: &str = "3333";

// ---------------------------------------------------------------------------
// Mock REST double (compact mirror of crates/discord/tests/common/mod.rs —
// that file is dev-only inside the discord crate's test tree and cannot be
// imported here, so the scripted-queue TCP double is reproduced).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RestRequest {
    method: String,
    path: String,
    body: Vec<u8>,
    received_at: std::time::Instant,
}

struct RestResponse {
    status: u16,
    body: Option<String>,
    delay: Duration,
}

impl RestResponse {
    fn status(status: u16) -> Self {
        Self {
            status,
            body: None,
            delay: Duration::ZERO,
        }
    }
}

struct MockRest {
    recorded: Arc<Mutex<Vec<RestRequest>>>,
    handle: JoinHandle<()>,
}

impl MockRest {
    /// `script` statuses are consumed in request order; once empty every
    /// request gets 200 with `{"id": "<counter>"}` for message posts (a real
    /// snowflake the store can record) or `{}` otherwise.
    async fn start(script: Vec<u16>) -> (Self, String) {
        Self::start_script(script.into_iter().map(RestResponse::status).collect()).await
    }

    async fn start_script(script: Vec<RestResponse>) -> (Self, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock listen");
        let origin = format!("http://{}", listener.local_addr().expect("addr"));
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(script.into_iter().collect::<VecDeque<_>>()));
        let posts = Arc::new(AtomicU64::new(0));
        let (rec, scr, ctr) = (recorded.clone(), script.clone(), posts.clone());
        let handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (rec, scr, ctr) = (rec.clone(), scr.clone(), ctr.clone());
                tokio::spawn(async move {
                    let Some(request) = read_rest_request(&mut stream).await else {
                        return;
                    };
                    rec.lock().expect("recorded").push(RestRequest {
                        method: request.0.clone(),
                        path: request.1.clone(),
                        body: request.2,
                        received_at: std::time::Instant::now(),
                    });
                    let response = scr
                        .lock()
                        .expect("script")
                        .pop_front()
                        .unwrap_or_else(|| RestResponse::status(200));
                    tokio::time::sleep(response.delay).await;
                    let status = response.status;
                    let body = response.body.unwrap_or_else(|| {
                        if status == 200 && request.0 == "POST" && request.1.ends_with("/messages")
                        {
                            let n = ctr.fetch_add(1, Ordering::Relaxed);
                            format!("{{\"id\":\"{}\"}}", 9_000_000_000_000_000_000u64 + n)
                        } else {
                            "{}".to_owned()
                        }
                    });
                    let reason = match status {
                        200 => "OK",
                        201 => "Created",
                        400 => "Bad Request",
                        403 => "Forbidden",
                        404 => "Not Found",
                        429 => "Too Many Requests",
                        _ => "Internal Server Error",
                    };
                    let head = format!(
                        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(body.as_bytes()).await;
                });
            }
        });
        (Self { recorded, handle }, origin)
    }

    fn requests(&self) -> Vec<RestRequest> {
        self.recorded.lock().expect("recorded").clone()
    }

    async fn posts_to(&self, suffix: &str) -> Vec<RestRequest> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path.ends_with(suffix))
            .collect()
    }

    fn deletes(&self) -> Vec<RestRequest> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "DELETE")
            .collect()
    }

    fn deferred_reply(&self) -> serde_json::Value {
        let requests = self.requests();
        let callbacks: Vec<_> = requests
            .iter()
            .filter(|r| r.method == "POST" && r.path.ends_with("/callback"))
            .collect();
        assert_eq!(callbacks.len(), 1, "exactly one initial acknowledgement");
        assert_eq!(requests[0].path, callbacks[0].path, "defer before effects");
        let json: serde_json::Value = serde_json::from_slice(&callbacks[0].body).unwrap();
        assert_eq!(json["type"], 5);
        assert_eq!(json["data"]["flags"], 64, "ephemeral defer");
        let edits: Vec<_> = requests.iter().filter(|r| r.method == "PATCH").collect();
        assert_eq!(edits.len(), 1, "one completion, not a second callback");
        assert_eq!(
            edits[0].path,
            "/api/v10/webhooks/1111/sticky-test-token/messages/@original"
        );
        let reply: serde_json::Value = serde_json::from_slice(&edits[0].body).unwrap();
        assert_eq!(reply["allowed_mentions"]["parse"], serde_json::json!([]));
        reply
    }

    async fn shutdown(self) {
        self.handle.abort();
    }
}

/// Read one HTTP/1.1 request (request line, headers, content-length body).
async fn read_rest_request(stream: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
        let n = stream.read_buf(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf.split_off(header_end + 4);
    while body.len() < content_length {
        if stream.read_buf(&mut body).await.ok()? == 0 {
            break;
        }
    }
    body.truncate(content_length);
    Some((method, path, body))
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn gates(automations: bool, announcements: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
        scorecard: false,
        automations,
        announcements,
        moderation: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn user(id: u64, bot: bool) -> User {
    User {
        accent_color: None,
        avatar: None,
        avatar_decoration: None,
        avatar_decoration_data: None,
        banner: None,
        bot,
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

fn message(id: u64, channel: u64, bot: bool, guild: Option<u64>) -> Message {
    Message {
        activity: None,
        application: None,
        application_id: None,
        attachments: vec![],
        author: user(77, bot),
        call: None,
        channel_id: Id::new(channel),
        components: vec![],
        content: "member activity".to_owned(),
        edited_timestamp: None,
        embeds: vec![],
        flags: Some(MessageFlags::empty()),
        guild_id: guild.map(Id::new),
        id: Id::new(4_000_000_000_000_000_000 + id),
        // `interaction` is deprecated in favour of `interaction_metadata`
        // but still a required literal field on 0.17.1.
        #[allow(deprecated)]
        interaction: None,
        interaction_metadata: None,
        kind: MessageType::Regular,
        member: None,
        mention_channels: vec![],
        mention_everyone: false,
        mention_roles: vec![],
        mentions: vec![],
        message_snapshots: vec![],
        pinned: false,
        poll: None,
        reactions: vec![],
        reference: None,
        referenced_message: None,
        role_subscription_data: None,
        sticker_items: vec![],
        timestamp: Timestamp::from_str("2026-09-30T12:00:00.000+00:00").expect("stamp"),
        thread: None,
        tts: false,
        webhook_id: None,
    }
}

#[allow(deprecated)]
fn slash(name: &str, channel: Option<u64>, options: Vec<CommandDataOption>) -> Interaction {
    Interaction {
        app_permissions: None,
        application_id: Id::new(1111),
        authorizing_integration_owners: ApplicationIntegrationMap {
            guild: Some(AnonymizableId::Id(Id::new(GUILD))),
            user: None,
        },
        channel: None,
        channel_id: channel.map(Id::new),
        context: None,
        data: Some(InteractionData::ApplicationCommand(Box::new(CommandData {
            guild_id: None,
            id: Id::new(1),
            name: name.to_owned(),
            kind: CommandType::ChatInput,
            options,
            resolved: None,
            target_id: None,
        }))),
        entitlements: Vec::new(),
        guild: None,
        guild_id: Some(Id::new(GUILD)),
        guild_locale: None,
        id: Id::new(7),
        kind: InteractionType::ApplicationCommand,
        locale: None,
        member: Some(twilight_model::guild::PartialMember {
            avatar: None,
            avatar_decoration_data: None,
            banner: None,
            communication_disabled_until: None,
            deaf: false,
            flags: MemberFlags::empty(),
            joined_at: None,
            mute: false,
            nick: None,
            permissions: Some(Permissions::MANAGE_GUILD),
            premium_since: None,
            roles: Vec::new(),
            user: Some(user(88, false)),
        }),
        message: None,
        token: "sticky-test-token".to_owned(),
        user: Some(user(99, false)),
    }
}

fn option(name: &str, value: CommandOptionValue) -> CommandDataOption {
    CommandDataOption {
        name: name.to_owned(),
        value,
    }
}

/// Runtime over a never-used lazy pool and a mock executor — for the gate
/// and routing paths that return before any DB or real-REST work.
fn runtime_without_db(
    gates: RouterGates,
    automations: bool,
    origin: String,
) -> Arc<CommandRuntime> {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    let executor =
        ActionExecutor::with_proxy("test-token".to_owned(), Some(origin)).expect("mock executor");
    CommandRuntime::new(
        pool,
        executor,
        router_with_commands(gates),
        GUILD,
        automations,
    )
}

/// READY payload: application 1111 owns the published guild registry.
fn ready() -> Event {
    Event::Ready(Ready {
        application: PartialApplication {
            flags: ApplicationFlags::empty(),
            id: Id::new(1111),
        },
        guilds: vec![UnavailableGuild {
            id: Id::new(GUILD),
            unavailable: true,
        }],
        resume_gateway_url: "wss://gateway.discord.gg".to_owned(),
        session_id: "test-session".to_owned(),
        shard: None,
        user: CurrentUser {
            accent_color: None,
            avatar: None,
            banner: None,
            bot: true,
            discriminator: 0,
            email: None,
            flags: None,
            global_name: None,
            id: Id::new(1111),
            locale: None,
            mfa_enabled: false,
            name: "two-bot".to_owned(),
            premium_type: None,
            public_flags: None,
            verified: None,
        },
        version: 10,
    })
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

// ---------------------------------------------------------------------------
// Units (no database)
// ---------------------------------------------------------------------------

/// Exercises the same narrowed composition as `from_env`, with no env races,
/// database connection, or external Discord calls.
fn activation_runtime(guild: u64, token: &str, origin: String) -> Arc<CommandRuntime> {
    let activation = crate::activation::BootActivation::from_token(Some(guild), Some(token));
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/agent_test")
        .expect("unused lazy test pool");
    let executor = ActionExecutor::with_proxy(token.to_owned(), Some(origin)).unwrap();
    let requested = RouterGates {
        configured_guild: Some(guild),
        automations: true,
        announcements: true,
        moderation: true,
        self_roles: true,
        ..gates(false, false)
    };
    CommandRuntime::from_gates(pool, executor, requested, &activation)
}

#[tokio::test]
async fn activation_boot_publishes_only_permitted_capabilities() {
    use two_bot_core::{
        activation::LiveCapability, announcement_commands, automation_commands,
        moderation_commands, ComponentHandler, ComponentOutcome, HandlerId,
    };
    const STAGING: u64 = 1545644954272137297;
    const LIVE: u64 = 326474832151838730;
    // Public ids encoded in synthetic credentials for the local double only.
    const STAGING_TOKEN: &str = "MTQ2OTEzNzYzNjY2Mzc1ODg4OA.mock.signature";
    const LIVE_TOKEN: &str = "MTUzOTcxMTY4Mzg5ODExODE1NA.mock.signature";
    const THIRD_TOKEN: &str = "MTU1NTU1NTU1NTU1NTU1NTU1Ng.mock.signature";
    for (guild, token, app, staging, self_roles) in [
        (STAGING, STAGING_TOKEN, 1469137636663758888, true, true),
        (LIVE, LIVE_TOKEN, 1539711683898118154, false, true),
        (LIVE, STAGING_TOKEN, 1469137636663758888, false, false),
        (STAGING, LIVE_TOKEN, 1539711683898118154, false, false),
        (
            1555555555555555555,
            STAGING_TOKEN,
            1469137636663758888,
            false,
            false,
        ),
        (STAGING, THIRD_TOKEN, 1555555555555555556, false, false),
    ]
    .into_iter()
    .flat_map(|(guild, token, app, staging, self_roles)| {
        [token.to_owned(), format!("Bot {token}")]
            .map(|token| (guild, token, app, staging, self_roles))
    }) {
        let (mock, origin) = MockRest::start(Vec::new()).await;
        let runtime = activation_runtime(guild, &token, origin);
        runtime.publish_registry(Some(app)).await;
        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "one full replacement, guild={guild}");
        assert_eq!(requests[0].method, "PUT");
        assert_eq!(
            requests[0].path,
            format!("/api/v10/applications/{app}/guilds/{guild}/commands")
        );
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let names: Vec<_> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|command| command["name"].as_str().unwrap())
            .collect();
        for definition in automation_commands()
            .into_iter()
            .chain(announcement_commands())
            .chain(moderation_commands())
        {
            assert_eq!(
                names.contains(&definition.name.as_str()),
                staging,
                "capability command {}, guild={guild}, app={app}",
                definition.name
            );
        }
        for handler in [
            HandlerId::AutomationAdmin,
            HandlerId::FeedAdd,
            HandlerId::FeedRemove,
            HandlerId::FeedList,
        ] {
            assert_eq!(runtime.router().handler_for(&handler).is_some(), staging);
        }
        assert_eq!(
            runtime
                .router()
                .route_component("two:self-role:fixture", Some(guild)),
            if self_roles {
                ComponentOutcome::Handled {
                    handler: ComponentHandler::SelfRole,
                }
            } else {
                ComponentOutcome::Ignore
            }
        );
        let activation = crate::activation::BootActivation::from_token(Some(guild), Some(&token));
        assert_eq!(activation.permitted(LiveCapability::Automod), staging);
        if !staging {
            // Even old sticky state cannot trigger the message hook. A lazy
            // test pool proves the hook exits before any database access.
            let msg = message(4444, CHANNEL, false, Some(guild));
            assert_eq!(runtime.on_message(&msg).await, ActivityOutcome::None);
            assert_eq!(mock.requests().len(), 1, "no sticky side effect");
        }
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn activation_boot_invalid_token_and_ready_identity_cannot_publish() {
    for token in ["not-a-token", "MTQ2OTEzNzYzNjY2Mzc1ODg4OA.mock.signature"] {
        let (mock, origin) = MockRest::start(Vec::new()).await;
        let runtime = activation_runtime(1545644954272137297, token, origin);
        runtime.publish_registry(Some(1539711683898118154)).await;
        assert!(
            mock.requests().is_empty(),
            "no PUT using an untrusted application id"
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn activation_boot_resumed_uses_current_clearance_and_checks_identity() {
    for (token, app) in [
        "MTUzOTcxMTY4Mzg5ODExODE1NA.mock.signature",
        "Bot MTUzOTcxMTY4Mzg5ODExODE1NA.mock.signature",
    ]
    .into_iter()
    .flat_map(|token| [1539711683898118154u64, 1469137636663758888u64].map(|app| (token, app)))
    {
        let (mock, origin) = MockRest::start_script(vec![RestResponse {
            status: 200,
            body: Some(format!("{{\"id\":\"{app}\"}}")),
            delay: Duration::ZERO,
        }])
        .await;
        let runtime = activation_runtime(326474832151838730, token, origin);
        runtime.publish_registry(None).await;
        let requests = mock.requests();
        assert_eq!(requests[0].method, "GET");
        assert_eq!(
            requests.len(),
            if app == 1539711683898118154 { 2 } else { 1 }
        );
        if requests.len() == 2 {
            let body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
            let names: Vec<_> = body
                .as_array()
                .unwrap()
                .iter()
                .map(|command| command["name"].as_str().unwrap())
                .collect();
            assert_eq!(
                names,
                vec!["rank", "leaderboard"],
                "unrelated core commands remain; uncleared surfaces are replaced"
            );
        }
        mock.shutdown().await;
    }
}

/// Use child test processes rather than mutating the parallel suite's env.
#[test]
fn activation_boot_from_env_isolates_denied_moderation_validation() {
    for (guild, token, owen, expected) in [
        (
            "326474832151838730",
            "MTUzOTcxMTY4Mzg5ODExODE1NA.mock.signature",
            "",
            "narrowed",
        ),
        (
            "326474832151838730",
            "MTQ2OTEzNzYzNjY2Mzc1ODg4OA.mock.signature",
            "",
            "narrowed",
        ),
        (
            "1545644954272137297",
            "MTQ2OTEzNzYzNjY2Mzc1ODg4OA.mock.signature",
            "",
            "invalid",
        ),
        (
            "1545644954272137297",
            "MTQ2OTEzNzYzNjY2Mzc1ODg4OA.mock.signature",
            "123456789012345678",
            "enabled",
        ),
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "command_runtime_tests::activation_boot_from_env_fixture",
                "--nocapture",
            ])
            .env_clear()
            .env("ACTIVATION_ENV_TEST_EXPECTED", expected)
            .env("GUILD_ID", guild)
            .env("DISCORD_TOKEN", token)
            .env("DISCORD_API_BASE", "http://127.0.0.1:9")
            .env("TWO_MODERATION", "1")
            .env("TWO_OWEN_USER_ID", owen)
            .env(
                "TWO_MODERATION_PROTECTED_ROLE_IDS",
                if expected == "narrowed" {
                    "invalid"
                } else {
                    ""
                },
            )
            .env("TWO_AUTOMOD", "1")
            .env("TWO_AUTOMATIONS", "1")
            .env("TWO_ANNOUNCEMENTS", "1")
            .output()
            .expect("isolated boot env fixture");
        assert!(
            output.status.success(),
            "fixture {expected}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }
}

#[tokio::test]
async fn activation_boot_from_env_fixture() {
    let Ok(expected) = std::env::var("ACTIVATION_ENV_TEST_EXPECTED") else {
        return;
    };
    let config = two_bot_core::Config::from_env().unwrap();
    let guild = config.guild_id.unwrap();
    let token = config.discord_token.as_ref().unwrap().expose();
    let activation = crate::activation::BootActivation::from_config(&config);
    assert_eq!(
        crate::gateway::intents_from_env(&activation)
            .contains(twilight_gateway::Intents::MESSAGE_CONTENT),
        expected != "narrowed"
    );
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/agent_test")
        .unwrap();
    let runtime = CommandRuntime::from_env(pool, token, guild, &activation);
    if expected == "invalid" {
        assert!(
            runtime.is_none(),
            "permitted staging moderation still validates Owen"
        );
        return;
    }
    let runtime = runtime.expect("denied moderation must not disable unrelated commands");
    let defs = runtime.router().publish_set(&[]).unwrap();
    let names: Vec<_> = defs.iter().map(|def| def.name.as_str()).collect();
    if expected == "narrowed" {
        assert_eq!(names, ["rank", "leaderboard"]);
        assert!(!runtime.router().gates().moderation);
        assert!(!runtime.router().gates().automations);
        assert!(!runtime.router().gates().announcements);
        assert!(
            !runtime.router().gates().self_roles,
            "permission never enables an unconfigured surface"
        );
    } else {
        assert!(runtime.router().gates().moderation);
        assert!(names.contains(&"ban"));
        assert!(names.contains(&"sticky"));
    }
}

#[test]
fn sticky_options_extract_body_and_debounce() {
    let interaction = slash(
        "sticky",
        Some(CHANNEL),
        vec![
            option("body", CommandOptionValue::String("remember".to_owned())),
            option("debounce", CommandOptionValue::Integer(7)),
        ],
    );
    assert_eq!(
        sticky_options(&interaction),
        (Some("remember".to_owned()), Some(7))
    );
}

#[test]
fn sticky_options_missing_options_validate_to_rejection_inputs() {
    let interaction = slash("sticky", Some(CHANNEL), Vec::new());
    assert_eq!(sticky_options(&interaction), (None, None));
}

#[test]
fn ephemeral_reply_shape_matches_router_refusals() {
    let response = ephemeral("hello");
    assert_eq!(
        response.kind,
        InteractionResponseType::ChannelMessageWithSource
    );
    let data = response.data.expect("data");
    assert_eq!(data.content.as_deref(), Some("hello"));
    assert_eq!(data.flags, Some(MessageFlags::EPHEMERAL));
}

#[test]
fn actor_id_prefers_member_user_then_top_level_user() {
    let with_member = slash("sticky", Some(CHANNEL), Vec::new());
    assert_eq!(actor_id(&with_member), "88");
    let mut dm = slash("sticky", Some(CHANNEL), Vec::new());
    dm.member = None;
    assert_eq!(actor_id(&dm), "99");
    dm.user = None;
    assert_eq!(actor_id(&dm), "");
}

#[tokio::test]
async fn message_gate_short_circuits_before_db_and_rest() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    // Bot author, wrong guild and automations-off each exit before touching
    // the lazy pool or the wire.
    for (automations, bot, guild) in [
        (true, true, Some(GUILD)),
        (true, false, Some(9999)),
        (false, false, Some(GUILD)),
    ] {
        let runtime = runtime_without_db(gates(true, false), automations, origin.clone());
        let outcome = runtime.on_message(&message(1, CHANNEL, bot, guild)).await;
        assert_eq!(outcome, ActivityOutcome::None);
    }
    assert!(mock.requests().is_empty(), "no Discord calls on gate-outs");
    mock.shutdown().await;
}

#[tokio::test]
async fn refused_interaction_is_answered_ephemerally_via_executor() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    // automations OFF in the router gates → the shared router refuses; the
    // runtime answers through the shared executor without DB work.
    let runtime = runtime_without_db(gates(false, false), false, origin);
    runtime
        .on_interaction(&slash("sticky", Some(CHANNEL), Vec::new()))
        .await;
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 1, "one interaction callback");
    assert_eq!(
        callbacks[0].path,
        "/api/v10/interactions/7/sticky-test-token/callback"
    );
    let json: serde_json::Value =
        serde_json::from_slice(&callbacks[0].body).expect("callback json");
    assert_eq!(json["type"], 4);
    assert_eq!(json["data"]["content"], AUTOMATIONS_DISABLED_REPLY);
    assert_eq!(json["data"]["flags"], 64, "ephemeral");
    mock.shutdown().await;
}

#[tokio::test]
async fn published_unwired_commands_reply_without_defer_or_store_work() {
    for router_gates in [
        gates(false, false),
        RouterGates {
            scorecard: true,
            moderation: true,
            ..gates(true, true)
        },
    ] {
        let (mock, origin) = MockRest::start(Vec::new()).await;
        let runtime = runtime_without_db(router_gates, router_gates.automations, origin);
        runtime.publish_registry(Some(1111)).await;
        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "one full registry publication");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let unwired: Vec<&str> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|command| command["name"].as_str().unwrap())
            .filter(|name| {
                !matches!(
                    *name,
                    "sticky" | "sticky-remove" | "feed-add" | "feed-remove" | "feed-list"
                )
            })
            .collect();
        assert!(
            unwired.contains(&"rank"),
            "rank publishes even with all gates off"
        );
        if router_gates.announcements {
            assert!(unwired.contains(&"rsvp"), "enabled unwired announcement");
        }
        for name in &unwired {
            let mut interaction = slash(name, Some(CHANNEL), Vec::new());
            interaction.member.as_mut().unwrap().permissions = Some(Permissions::all());
            runtime.on_interaction(&interaction).await;
        }
        let callbacks = mock.posts_to("/callback").await;
        assert_eq!(
            callbacks.len(),
            unwired.len(),
            "each published unwired name replies"
        );
        for callback in callbacks {
            let reply: serde_json::Value = serde_json::from_slice(&callback.body).unwrap();
            assert_eq!(reply["type"], 4, "immediate response, not a defer");
            assert_eq!(reply["data"]["flags"], 64);
            assert_eq!(
                reply["data"]["content"],
                "This command is not available in this build yet."
            );
        }
        assert_eq!(
            mock.requests().len(),
            1 + unwired.len(),
            "no other REST effects"
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn unwired_commands_preserve_disabled_and_permission_refusals() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    // A stale picker entry after a gate transition must still receive its
    // existing refusal, before considering runtime availability.
    for (router_gates, name, permitted, expected) in [
        (
            gates(false, false),
            "schedule",
            true,
            AUTOMATIONS_DISABLED_REPLY,
        ),
        (
            gates(true, false),
            "rsvp",
            true,
            ANNOUNCEMENTS_DISABLED_REPLY,
        ),
        (gates(true, true), "command", false, MANAGE_SERVER_REQUIRED),
    ] {
        let runtime = runtime_without_db(router_gates, router_gates.automations, origin.clone());
        let mut interaction = slash(name, Some(CHANNEL), Vec::new());
        if !permitted {
            interaction.member.as_mut().unwrap().permissions = Some(Permissions::empty());
        }
        runtime.on_interaction(&interaction).await;
        let callbacks = mock.posts_to("/callback").await;
        let reply: serde_json::Value =
            serde_json::from_slice(&callbacks.last().unwrap().body).unwrap();
        assert_eq!(reply["type"], 4);
        assert_eq!(reply["data"]["flags"], 64);
        assert_eq!(reply["data"]["content"], expected);
    }
    assert_eq!(mock.requests().len(), 3, "only refusal callbacks");
    mock.shutdown().await;
}

#[tokio::test]
async fn unknown_and_foreign_non_moderation_commands_remain_silent() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash("not-a-command", Some(CHANNEL), Vec::new()))
        .await;
    for name in ["rank", "rsvp", "command", "schedule"] {
        for guild in [Some(Id::new(9999)), None] {
            let mut interaction = slash(name, Some(CHANNEL), Vec::new());
            interaction.guild_id = guild;
            runtime.on_interaction(&interaction).await;
        }
    }
    assert!(mock.requests().is_empty(), "router Ignore stays silent");
    mock.shutdown().await;
}

#[tokio::test]
async fn foreign_moderation_preserves_the_router_guild_refusal() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(false, false), false, origin);
    let mut interaction = slash("ban", Some(CHANNEL), Vec::new());
    interaction.guild_id = Some(Id::new(9999));
    runtime.on_interaction(&interaction).await;
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 1);
    let reply: serde_json::Value = serde_json::from_slice(&callbacks[0].body).unwrap();
    assert_eq!(reply["type"], 4);
    assert_eq!(reply["data"]["flags"], 64);
    assert_eq!(
        reply["data"]["content"],
        RouterRefusal::GuildRestricted.message()
    );
    assert_eq!(mock.requests().len(), 1, "no foreign-guild effects");
    mock.shutdown().await;
}

#[tokio::test]
async fn dispatch_spawns_interaction_work_off_the_shard_loop() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(false, false), false, origin);
    let interaction = slash("sticky-remove", Some(CHANNEL), Vec::new());
    runtime.dispatch(&Event::InteractionCreate(Box::new(InteractionCreate(
        interaction,
    ))));
    // The spawned task must reach Discord without the test awaiting it.
    wait_for(
        || !mock.requests().is_empty(),
        "refusal callback via dispatch",
    )
    .await;
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 1);
    mock.shutdown().await;
}

// ---------------------------------------------------------------------------
// Feed slice units (no database)
// ---------------------------------------------------------------------------

#[test]
fn feed_add_options_extract_kind_and_source() {
    let interaction = slash(
        "feed-add",
        Some(CHANNEL),
        vec![
            option("kind", CommandOptionValue::String("rss".to_owned())),
            option(
                "source",
                CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
            ),
        ],
    );
    assert_eq!(
        feed_add_options(&interaction),
        (
            Some("rss".to_owned()),
            Some("https://example.com/feed.xml".to_owned())
        )
    );
}

#[test]
fn feed_add_options_missing_values_decode_to_none() {
    let interaction = slash("feed-add", Some(CHANNEL), Vec::new());
    assert_eq!(feed_add_options(&interaction), (None, None));
}

#[test]
fn feed_remove_option_extracts_id() {
    let interaction = slash(
        "feed-remove",
        Some(CHANNEL),
        vec![option(
            "id",
            CommandOptionValue::String("feed-1".to_owned()),
        )],
    );
    assert_eq!(feed_remove_option(&interaction), Some("feed-1".to_owned()));
    let missing = slash("feed-remove", Some(CHANNEL), Vec::new());
    assert_eq!(feed_remove_option(&missing), None);
}

#[test]
fn new_id_is_unique_hex_that_validate_id_accepts() {
    let first = new_id();
    let second = new_id();
    assert_eq!(first.len(), 32);
    assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_ne!(first, second);
}

#[tokio::test]
async fn feed_commands_refuse_ephemerally_while_announcements_off() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    // announcements OFF in the router gates → the shared router refuses all
    // three feed commands; the runtime answers through the shared executor.
    let runtime = runtime_without_db(gates(true, false), true, origin);
    for name in ["feed-add", "feed-remove", "feed-list"] {
        runtime
            .on_interaction(&slash(name, Some(CHANNEL), Vec::new()))
            .await;
    }
    let requests = mock.requests();
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 3, "one ephemeral refusal per command");
    assert_eq!(
        requests.len(),
        3,
        "no defer, no edit, no publish: {requests:?}"
    );
    for callback in &callbacks {
        let json: serde_json::Value =
            serde_json::from_slice(&callback.body).expect("callback json");
        assert_eq!(json["type"], 4);
        assert_eq!(json["data"]["content"], ANNOUNCEMENTS_DISABLED_REPLY);
        assert_eq!(json["data"]["flags"], 64, "ephemeral");
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn feed_add_without_manage_server_is_refused() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    let mut interaction = slash(
        "feed-add",
        Some(CHANNEL),
        vec![
            option("kind", CommandOptionValue::String("rss".to_owned())),
            option(
                "source",
                CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
            ),
        ],
    );
    interaction.member.as_mut().unwrap().permissions = Some(Permissions::empty());
    runtime.on_interaction(&interaction).await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "refusal only; no defer or mutation");
    let json: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("callback json");
    assert_eq!(json["type"], 4);
    assert_eq!(json["data"]["content"], MANAGE_SERVER_REQUIRED);
    assert_eq!(json["data"]["flags"], 64);
    mock.shutdown().await;
}

#[tokio::test]
async fn feed_commands_from_other_guilds_are_silent() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    for name in ["feed-add", "feed-remove", "feed-list"] {
        let mut interaction = slash(name, Some(CHANNEL), Vec::new());
        interaction.guild_id = Some(Id::new(9999));
        runtime.on_interaction(&interaction).await;
    }
    assert!(
        mock.requests().is_empty(),
        "wrong-guild interactions get silence, not a refusal"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn feed_add_with_unknown_kind_replies_domain_error() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash(
            "feed-add",
            Some(CHANNEL),
            vec![
                option("kind", CommandOptionValue::String("atom".to_owned())),
                option(
                    "source",
                    CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
                ),
            ],
        ))
        .await;
    // Kind decodes before planning: the defer+edit envelope still applies.
    assert_eq!(mock.deferred_reply()["content"], "Unknown feed kind.");
    mock.shutdown().await;
}

#[tokio::test]
async fn feed_add_with_non_https_source_replies_ssrf_guard_error() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash(
            "feed-add",
            Some(CHANNEL),
            vec![
                option("kind", CommandOptionValue::String("rss".to_owned())),
                option(
                    "source",
                    CommandOptionValue::String("http://169.254.169.254/latest".to_owned()),
                ),
            ],
        ))
        .await;
    assert_eq!(
        mock.deferred_reply()["content"],
        "Feed source must be an HTTPS URL without embedded credentials."
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn feed_remove_without_id_replies_invalid_id() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash("feed-remove", Some(CHANNEL), Vec::new()))
        .await;
    assert_eq!(mock.deferred_reply()["content"], "Invalid feed id.");
    mock.shutdown().await;
}

#[tokio::test]
async fn feed_add_stops_when_the_defer_fails() {
    let (mock, origin) = MockRest::start(vec![404]).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash(
            "feed-add",
            Some(CHANNEL),
            vec![
                option("kind", CommandOptionValue::String("rss".to_owned())),
                option(
                    "source",
                    CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
                ),
            ],
        ))
        .await;
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        1,
        "the failed defer is the only call — no reply path, no mutation"
    );
    assert!(requests[0].path.ends_with("/callback"));
    mock.shutdown().await;
}

#[tokio::test]
async fn ready_publishes_the_complete_merged_registry_once() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime.dispatch(&ready());
    wait_for(
        || mock.requests().iter().any(|r| r.method == "PUT"),
        "registry publish PUT",
    )
    .await;
    let puts: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PUT")
        .collect();
    assert_eq!(puts.len(), 1, "one full-set PUT per READY");
    assert_eq!(
        puts[0].path,
        "/api/v10/applications/1111/guilds/2222/commands"
    );
    let body: serde_json::Value = serde_json::from_slice(&puts[0].body).expect("publish body");
    let names: Vec<&str> = body
        .as_array()
        .expect("command array")
        .iter()
        .map(|command| command["name"].as_str().expect("name"))
        .collect();
    for expected in [
        "rank",
        "leaderboard",
        "command",
        "command-remove",
        "command-list",
        "schedule",
        "schedule-remove",
        "schedule-list",
        "sticky",
        "sticky-remove",
        "rsvp",
        "rsvp-attendance",
        "lfg",
        "lfg-close",
        "feed-add",
        "feed-remove",
        "feed-list",
    ] {
        assert!(
            names.contains(&expected),
            "published set is missing {expected}"
        );
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn ready_publish_withholds_gated_off_feed_commands() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    // announcements OFF: the rest of the merged registry still publishes, but
    // the announcement group (feeds included) is withheld — not feeds-only.
    let runtime = runtime_without_db(gates(true, false), true, origin);
    runtime.dispatch(&ready());
    wait_for(
        || mock.requests().iter().any(|r| r.method == "PUT"),
        "registry publish PUT",
    )
    .await;
    let puts: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PUT")
        .collect();
    assert_eq!(puts.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&puts[0].body).expect("publish body");
    let names: Vec<&str> = body
        .as_array()
        .expect("command array")
        .iter()
        .map(|command| command["name"].as_str().expect("name"))
        .collect();
    for expected in ["rank", "leaderboard", "sticky", "sticky-remove"] {
        assert!(
            names.contains(&expected),
            "published set is missing {expected}"
        );
    }
    for withheld in ["feed-add", "feed-remove", "feed-list", "rsvp", "lfg"] {
        assert!(
            !names.contains(&withheld),
            "gated-off {withheld} must not publish"
        );
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn duplicate_ready_republishes_the_identical_set() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime.dispatch(&ready());
    runtime.dispatch(&ready());
    wait_for(
        || mock.requests().iter().filter(|r| r.method == "PUT").count() == 2,
        "second registry publish PUT",
    )
    .await;
    let puts: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PUT")
        .collect();
    assert_eq!(puts.len(), 2);
    assert_eq!(
        puts[0].body, puts[1].body,
        "set_guild_commands is a full replace — the repeat is identical"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn resumed_boot_synchronizes_current_gates_once_without_ready() {
    // A saved gateway session carries no application id or old process gates.
    // Both transitions must replace the remote registry using the new gates.
    for announcements in [false, true] {
        let (mock, origin) = MockRest::start_script(vec![RestResponse {
            status: 200,
            body: Some(r#"{"id":"1111"}"#.to_owned()),
            delay: Duration::from_millis(50),
        }])
        .await;
        let runtime = runtime_without_db(gates(true, announcements), true, origin);
        runtime.dispatch(&Event::Resumed);
        runtime.dispatch(&Event::Resumed);
        wait_for(
            || mock.requests().iter().any(|r| r.method == "PUT"),
            "resumed boot registry publish",
        )
        .await;
        // Wait behind the in-flight sync; this and the second RESUMED must
        // observe successful publication instead of issuing another lookup.
        runtime.publish_registry(None).await;
        let requests = mock.requests();
        assert_eq!(requests.len(), 2, "one lookup and one full replacement");
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/api/v10/applications/@me");
        assert_eq!(requests[1].method, "PUT");
        assert_eq!(
            requests[1].path,
            "/api/v10/applications/1111/guilds/2222/commands"
        );
        let body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        let names: Vec<&str> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|command| command["name"].as_str().unwrap())
            .collect();
        for name in ["rank", "leaderboard", "sticky", "sticky-remove"] {
            assert!(names.contains(&name), "resumed registry is complete");
        }
        for name in ["feed-add", "feed-remove", "feed-list", "rsvp", "lfg"] {
            assert_eq!(
                names.contains(&name),
                announcements,
                "current gates for {name}"
            );
        }
        runtime.publish_registry(Some(1111)).await;
        let puts: Vec<_> = mock
            .requests()
            .into_iter()
            .filter(|r| r.method == "PUT")
            .collect();
        assert_eq!(puts.len(), 2, "READY still republishes after RESUMED");
        assert_eq!(puts[0].body, puts[1].body, "identical complete set");
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn ready_sync_makes_later_resumed_connections_no_ops() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime.publish_registry(Some(1111)).await;
    runtime.publish_registry(None).await;
    runtime.publish_registry(None).await;
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        1,
        "no application lookup or redundant resume PUT"
    );
    assert_eq!(requests[0].method, "PUT");
    mock.shutdown().await;
}

#[tokio::test]
async fn resumed_lookup_failure_or_invalid_id_never_guesses_a_publish_target() {
    for (status, body) in [
        (403, "{}"),
        (200, "not json"),
        (200, r#"{"id":"0"}"#),
        (200, r#"{"id":"not-a-snowflake"}"#),
        (200, "{}"),
    ] {
        let (mock, origin) = MockRest::start_script(vec![RestResponse {
            status,
            body: Some(body.to_owned()),
            delay: Duration::ZERO,
        }])
        .await;
        let runtime = runtime_without_db(gates(true, true), true, origin);
        runtime.publish_registry(None).await;
        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "failed discovery has no PUT");
        assert_eq!(requests[0].method, "GET");
        runtime.publish_registry(Some(1111)).await;
        runtime.publish_registry(None).await;
        assert_eq!(mock.requests().len(), 2, "later READY can synchronize");
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn failed_resumed_publication_can_retry_on_a_later_connection() {
    let application = || RestResponse {
        status: 200,
        body: Some(r#"{"id":"1111"}"#.to_owned()),
        delay: Duration::ZERO,
    };
    let (mock, origin) = MockRest::start_script(vec![
        application(),
        RestResponse::status(403),
        application(),
        RestResponse::status(200),
    ])
    .await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime.publish_registry(None).await;
    runtime.publish_registry(None).await;
    runtime.publish_registry(None).await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 4, "retry only until one successful sync");
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[1].method, "PUT");
    assert_eq!(requests[2].method, "GET");
    assert_eq!(requests[3].method, "PUT");
    assert_eq!(requests[1].body, requests[3].body);
    mock.shutdown().await;
}

// ---------------------------------------------------------------------------
// agent-testdb acceptance (CI's Postgres service; never production/staging)
// ---------------------------------------------------------------------------

struct TestDb {
    pool: PgPool,
    admin: PgPool,
    schema: String,
}

impl TestDb {
    async fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let url = std::env::var("TWO_GATEWAY_TEST_DATABASE_URL")
            .expect("set the dedicated test URL; runtime DATABASE_URL is never used");
        let options = PgConnectOptions::from_str(&url).expect("test URL");
        assert!(matches!(
            options.get_host(),
            "agent-testdb" | "localhost" | "127.0.0.1"
        ));
        assert_eq!(options.get_username(), "agent_test");
        assert_eq!(options.get_database(), Some("agent_test"));
        // Unix sockets and `options=` query overrides would bypass the
        // host/credential allowlist above; reject both outright.
        assert!(options.get_socket().is_none());
        assert!(options.get_options().is_none());
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .expect("agent test database");
        let schema = format!(
            "sticky_test_{}_{}_{}",
            std::process::id(),
            now_millis_for_test(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
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

    /// Seed a sticky row directly so `last_message_id`/`last_posted_at` are
    /// controlled (`put_sticky` cannot express "previously posted").
    async fn seed(
        &self,
        last_message_id: Option<&str>,
        last_posted_ago_ms: i64,
        debounce_seconds: i32,
    ) {
        let now = now_millis_for_test();
        let posted_at = last_message_id.map(|_| now - last_posted_ago_ms);
        sqlx::query(
            "INSERT INTO sticky_messages
               (guild_id, channel_id, body, debounce_seconds, enabled,
                last_message_id, last_posted_at,
                created_by, created_at, updated_by, updated_at)
             VALUES ($1, $2, 'remember this', $3, TRUE, $4,
                     to_timestamp($5::DOUBLE PRECISION / 1000.0),
                     '77', to_timestamp($6::DOUBLE PRECISION / 1000.0),
                     '77', to_timestamp($6::DOUBLE PRECISION / 1000.0))",
        )
        .bind(GUILD_S)
        .bind(CHANNEL_S)
        .bind(debounce_seconds)
        .bind(last_message_id)
        .bind(posted_at)
        .bind(now)
        .execute(&self.pool)
        .await
        .expect("seed sticky");
    }

    async fn row(&self) -> Option<(String, Option<String>, Option<String>)> {
        sqlx::query_as(
            "SELECT body, last_message_id, claim_token FROM sticky_messages
              WHERE guild_id = $1 AND channel_id = $2",
        )
        .bind(GUILD_S)
        .bind(CHANNEL_S)
        .fetch_optional(&self.pool)
        .await
        .expect("sticky row")
    }

    async fn audits(&self) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT action, outcome FROM automation_audit_log
              WHERE guild_id = $1 ORDER BY created_at, id",
        )
        .bind(GUILD_S)
        .fetch_all(&self.pool)
        .await
        .expect("audit rows")
    }

    /// Seed a feed relay directly (other-guild isolation rows, missing-id
    /// targets) without going through the command path under test.
    async fn seed_feed(&self, guild_id: &str, id: &str, kind: &str) {
        // Distinct sources respect UNIQUE (guild_id, channel_id, kind, source).
        // created_at/updated_at are NOT NULL without defaults (0180_feeds.sql).
        sqlx::query(
            "INSERT INTO feed_relays
               (id, guild_id, channel_id, kind, source, created_by, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, '77',
                     clock_timestamp(), clock_timestamp())",
        )
        .bind(id)
        .bind(guild_id)
        .bind(CHANNEL_S)
        .bind(kind)
        .bind(format!("https://example.com/{id}.xml"))
        .execute(&self.pool)
        .await
        .expect("seed feed relay");
    }

    /// (id, guild_id, channel_id, kind, source) for this guild's relays.
    async fn feed_rows(&self) -> Vec<(String, String, String, String, String)> {
        sqlx::query_as(
            "SELECT id, guild_id, channel_id, kind, source FROM feed_relays
              WHERE guild_id = $1 ORDER BY created_at, id",
        )
        .bind(GUILD_S)
        .fetch_all(&self.pool)
        .await
        .expect("feed relay rows")
    }

    /// (action, target_key, outcome) from the shared announcements audit log.
    async fn feed_audits(&self) -> Vec<(String, String, String)> {
        sqlx::query_as(
            "SELECT action, target_key, outcome FROM announcements_audit_log
              WHERE guild_id = $1 ORDER BY created_at, id",
        )
        .bind(GUILD_S)
        .fetch_all(&self.pool)
        .await
        .expect("announcement audit rows")
    }
}

async fn db_runtime(db: &TestDb, script: Vec<u16>) -> (Arc<CommandRuntime>, MockRest) {
    db_runtime_script(db, script.into_iter().map(RestResponse::status).collect()).await
}

async fn db_runtime_script(
    db: &TestDb,
    script: Vec<RestResponse>,
) -> (Arc<CommandRuntime>, MockRest) {
    let (mock, origin) = MockRest::start_script(script).await;
    // Automations + announcements ON: one runtime serves the sticky slice and
    // the feed slice off the same router/executor/pool — the coexistence the
    // composition card requires.
    let runtime = CommandRuntime::new(
        db.pool.clone(),
        executor_at(origin),
        router_with_commands(gates(true, true)),
        GUILD,
        true,
    );
    (runtime, mock)
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn slow_cleanup_is_acknowledged_before_deadline_and_completed_by_edit() {
    let db = TestDb::new().await;
    db.seed(Some("8000000000000000005"), 120_000, 5).await;
    let (runtime, mock) = db_runtime_script(
        &db,
        vec![
            RestResponse::status(200),
            RestResponse {
                status: 200,
                body: None,
                delay: Duration::from_millis(3500),
            },
        ],
    )
    .await;
    let start = std::time::Instant::now();
    runtime
        .on_interaction(&slash("sticky-remove", Some(CHANNEL), vec![]))
        .await;
    assert_eq!(mock.deferred_reply()["content"], "Sticky removed.");
    let requests = mock.requests();
    assert!(requests[0].received_at.duration_since(start) < Duration::from_secs(3));
    assert_eq!(requests[1].method, "DELETE");
    assert_eq!(requests[2].method, "PATCH");
    assert!(
        requests[2]
            .received_at
            .duration_since(requests[1].received_at)
            >= Duration::from_millis(3500)
    );
    assert!(db.row().await.is_none());
    assert_eq!(
        db.audits().await,
        vec![("sticky.delete".into(), "ok".into())]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn failed_defer_leaves_set_and_remove_state_untouched() {
    let db = TestDb::new().await;
    db.seed(Some("8888"), 120_000, 5).await;
    let (runtime, mock) = db_runtime(&db, vec![404, 404]).await;
    for name in ["sticky", "sticky-remove"] {
        runtime
            .on_interaction(&slash(
                name,
                Some(CHANNEL),
                vec![option("body", CommandOptionValue::String("changed".into()))],
            ))
            .await;
    }
    assert_eq!(
        db.row().await,
        Some(("remember this".into(), Some("8888".into()), None))
    );
    assert!(db.audits().await.is_empty());
    assert_eq!(mock.requests().len(), 2);
    assert_eq!(mock.posts_to("/callback").await.len(), 2);
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn unconfirmed_replacement_preserves_previous_and_releases_claim() {
    let db = TestDb::new().await;
    db.seed(Some("8888"), 120_000, 5).await;
    for body in [
        "{}",
        "not JSON",
        r#"{"id":null}"#,
        r#"{"id":0}"#,
        r#"{"id":""}"#,
        r#"{"id":"0"}"#,
        r#"{"id":"abc"}"#,
        r#"{"id":"+123"}"#,
        r#"{"id":"18446744073709551616"}"#,
    ] {
        let (runtime, mock) = db_runtime_script(
            &db,
            vec![RestResponse {
                status: 200,
                body: Some(body.into()),
                delay: Duration::ZERO,
            }],
        )
        .await;
        assert_eq!(
            runtime
                .on_message(&message(24, CHANNEL, false, Some(GUILD)))
                .await,
            ActivityOutcome::Held,
            "{body}"
        );
        assert_eq!(
            db.row().await,
            Some(("remember this".into(), Some("8888".into()), None)),
            "{body}"
        );
        assert_eq!(mock.posts_to("/messages").await.len(), 1);
        assert!(mock.deletes().is_empty(), "no retirement for {body}");
        mock.shutdown().await;
    }
    let audits = db.audits().await;
    assert_eq!(audits.len(), 9);
    assert!(audits
        .iter()
        .all(|(action, outcome)| action == "sticky.run" && outcome == "post_failed"));
    let (runtime, mock) = db_runtime(&db, vec![]).await;
    assert_eq!(
        runtime
            .on_message(&message(25, CHANNEL, false, Some(GUILD)))
            .await,
        ActivityOutcome::Reposted
    );
    assert_eq!(
        mock.deletes().len(),
        1,
        "subsequent confirmed replacement retires previous"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn burst_coalesces_to_one_repost_and_retires_previous() {
    let db = TestDb::new().await;
    db.seed(Some("8000000000000000001"), 120_000, 5).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    // Three racing accepted messages: the atomic claim (and the debounce
    // clause on the post-record row) leave exactly one re-post.
    let outcomes = futures_util::future::join_all((0..3).map(|i| {
        let runtime = runtime.clone();
        async move {
            runtime
                .on_message(&message(10 + i, CHANNEL, false, Some(GUILD)))
                .await
        }
    }))
    .await;
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == ActivityOutcome::Reposted)
            .count(),
        1,
        "exactly one repost: {outcomes:?}"
    );

    let posts = mock.posts_to("/messages").await;
    assert_eq!(posts.len(), 1, "one re-post: {:?}", mock.requests());
    let posted: serde_json::Value = serde_json::from_slice(&posts[0].body).expect("post body");
    assert_eq!(posted["content"], "remember this");
    assert_eq!(posted["enforce_nonce"], true, "nonce dedupe rides along");

    // The previous sticky is retired best-effort after the record.
    let deletes = mock.deletes();
    assert_eq!(deletes.len(), 1);
    assert_eq!(
        deletes[0].path,
        "/api/v10/channels/3333/messages/8000000000000000001"
    );

    let (body, last_id, claim) = db.row().await.expect("row exists");
    assert_eq!(body, "remember this");
    assert_eq!(last_id.as_deref(), Some("9000000000000000000"));
    assert_eq!(claim, None, "record releases the claim");

    assert_eq!(
        db.audits().await,
        vec![("sticky.run".to_owned(), "ok".to_owned())]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn held_claim_blocks_the_second_attempt() {
    let db = TestDb::new().await;
    db.seed(Some("8000000000000000002"), 120_000, 5).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    // A foreign attempt holds the claim; the pure precheck can still say
    // Repost — only the store claim authorizes the wire call.
    let grant = store::claim_sticky_post(
        &db.pool,
        GUILD_S,
        CHANNEL_S,
        "foreign-claim",
        now_millis_for_test(),
    )
    .await
    .expect("claim")
    .expect("claim granted");
    assert_eq!(grant.body, "remember this");

    let outcome = runtime
        .on_message(&message(20, CHANNEL, false, Some(GUILD)))
        .await;
    assert_eq!(outcome, ActivityOutcome::Held);
    assert!(mock.posts_to("/messages").await.is_empty());
    assert!(db.audits().await.is_empty(), "held writes no audit row");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn debounce_window_holds_the_repost() {
    let db = TestDb::new().await;
    // Posted one second ago with a 5s debounce: inside the window.
    db.seed(Some("8000000000000000003"), 1_000, 5).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    let outcome = runtime
        .on_message(&message(21, CHANNEL, false, Some(GUILD)))
        .await;
    assert_eq!(outcome, ActivityOutcome::Held);
    assert!(mock.requests().is_empty());
    assert!(db.audits().await.is_empty());
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn post_failure_releases_claim_and_audits_post_failed() {
    let db = TestDb::new().await;
    db.seed(Some("8000000000000000004"), 120_000, 5).await;
    // First request (the replacement post) fails 500; default 200 after.
    let (runtime, mock) = db_runtime(&db, vec![500]).await;

    let outcome = runtime
        .on_message(&message(22, CHANNEL, false, Some(GUILD)))
        .await;
    assert_eq!(outcome, ActivityOutcome::Held);
    assert_eq!(
        db.audits().await,
        vec![("sticky.run".to_owned(), "post_failed".to_owned())]
    );
    let (_, last_id, claim) = db.row().await.expect("row exists");
    assert_eq!(last_id.as_deref(), Some("8000000000000000004"), "unchanged");
    assert_eq!(claim, None, "claim released for the next activity");

    // The released claim lets the very next activity re-post successfully.
    let outcome = runtime
        .on_message(&message(23, CHANNEL, false, Some(GUILD)))
        .await;
    assert_eq!(outcome, ActivityOutcome::Reposted);
    // Failed attempt + the retry both hit the wire; only the retry lands.
    assert_eq!(mock.posts_to("/messages").await.len(), 2);
    let audits = db.audits().await;
    assert_eq!(
        audits.last().map(|(a, o)| (a.as_str(), o.as_str())),
        Some(("sticky.run", "ok"))
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_remove_clears_state_and_deletes_the_message() {
    let db = TestDb::new().await;
    db.seed(Some("8000000000000000005"), 120_000, 5).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash("sticky-remove", Some(CHANNEL), Vec::new()))
        .await;

    assert!(db.row().await.is_none(), "row deleted");
    let deletes = mock.deletes();
    assert_eq!(deletes.len(), 1, "previous message cleaned up");
    assert_eq!(
        deletes[0].path,
        "/api/v10/channels/3333/messages/8000000000000000005"
    );
    let json = mock.deferred_reply();
    assert_eq!(json["content"], "Sticky removed.");
    assert_eq!(
        db.audits().await,
        vec![("sticky.delete".to_owned(), "ok".to_owned())]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_remove_absent_reports_absent_without_delete() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash("sticky-remove", Some(CHANNEL), Vec::new()))
        .await;

    assert!(mock.deletes().is_empty());
    let json = mock.deferred_reply();
    assert_eq!(json["content"], "No sticky in this channel.");
    assert_eq!(
        db.audits().await,
        vec![("sticky.delete".to_owned(), "absent".to_owned())]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_set_writes_row_audits_and_confirms() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash(
            "sticky",
            Some(CHANNEL),
            vec![
                option("body", CommandOptionValue::String("pin me".to_owned())),
                option("debounce", CommandOptionValue::Integer(7)),
            ],
        ))
        .await;

    let (body, _, _) = db.row().await.expect("row written");
    assert_eq!(body, "pin me");
    let debounce: i32 = sqlx::query_scalar(
        "SELECT debounce_seconds FROM sticky_messages
          WHERE guild_id = $1 AND channel_id = $2",
    )
    .bind(GUILD_S)
    .bind(CHANNEL_S)
    .fetch_one(&db.pool)
    .await
    .expect("debounce");
    assert_eq!(debounce, 7);
    let json = mock.deferred_reply();
    assert_eq!(json["content"], "Sticky set for <#3333>, 7s debounce.");
    assert_eq!(
        db.audits().await,
        vec![("sticky.create".to_owned(), "ok".to_owned())]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_set_rejection_audits_rejected_and_still_replies() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    // Empty body fails the 1–2000 UTF-16 validation.
    runtime
        .on_interaction(&slash(
            "sticky",
            Some(CHANNEL),
            vec![option("body", CommandOptionValue::String(String::new()))],
        ))
        .await;

    assert!(db.row().await.is_none(), "no row on rejection");
    assert!(mock.deferred_reply()["content"].as_str().is_some());
    assert_eq!(
        db.audits().await,
        vec![("sticky.create".to_owned(), "rejected".to_owned())]
    );
    mock.shutdown().await;
    db.close().await;
}

// --- feed slice: guild-scoped CRUD + announcements_audit_log ---------------

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_add_persists_relay_for_the_invoking_channel_and_audits() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash(
            "feed-add",
            Some(CHANNEL),
            vec![
                option("kind", CommandOptionValue::String("rss".to_owned())),
                option(
                    "source",
                    CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
                ),
            ],
        ))
        .await;

    let rows = db.feed_rows().await;
    assert_eq!(rows.len(), 1, "one relay row written");
    let (id, guild_id, channel_id, kind, source) = &rows[0];
    assert_eq!(guild_id, GUILD_S);
    assert_eq!(channel_id, CHANNEL_S, "relay binds the invoking channel");
    assert_eq!(kind, "rss");
    assert_eq!(source, "https://example.com/feed.xml");
    let created_by: String =
        sqlx::query_scalar("SELECT created_by FROM feed_relays WHERE guild_id = $1")
            .bind(GUILD_S)
            .fetch_one(&db.pool)
            .await
            .expect("created_by");
    assert_eq!(created_by, "88", "invoking member recorded");

    let json = mock.deferred_reply();
    assert_eq!(
        json["content"],
        format!("Feed relay created: `{id}`."),
        "ephemeral confirmation names the new id"
    );
    assert_eq!(
        db.feed_audits().await,
        vec![("feed.create".to_owned(), id.clone(), "rss".to_owned())]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_remove_deletes_present_and_reports_missing() {
    let db = TestDb::new().await;
    db.seed_feed(GUILD_S, "feed-keep", "rss").await;
    db.seed_feed(GUILD_S, "feed-drop", "rss").await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash(
            "feed-remove",
            Some(CHANNEL),
            vec![option(
                "id",
                CommandOptionValue::String("feed-drop".to_owned()),
            )],
        ))
        .await;
    runtime
        .on_interaction(&slash(
            "feed-remove",
            Some(CHANNEL),
            vec![option(
                "id",
                CommandOptionValue::String("feed-absent".to_owned()),
            )],
        ))
        .await;

    let rows = db.feed_rows().await;
    assert_eq!(
        rows.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(),
        vec!["feed-keep"],
        "only the targeted relay is deleted"
    );
    let edits: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH")
        .collect();
    assert_eq!(edits.len(), 2, "one completion per remove");
    let first: serde_json::Value = serde_json::from_slice(&edits[0].body).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&edits[1].body).unwrap();
    assert_eq!(first["content"], "Feed relay removed.");
    assert_eq!(second["content"], "No feed relay with that id.");
    assert_eq!(
        db.feed_audits().await,
        vec![
            (
                "feed.remove".to_owned(),
                "feed-drop".to_owned(),
                "removed".to_owned()
            ),
            (
                "feed.remove".to_owned(),
                "feed-absent".to_owned(),
                "missing".to_owned()
            ),
        ]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_list_is_scoped_to_the_configured_guild() {
    let db = TestDb::new().await;
    db.seed_feed(GUILD_S, "feed-own", "rss").await;
    db.seed_feed("9999", "feed-foreign", "twitch").await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash("feed-list", Some(CHANNEL), Vec::new()))
        .await;

    let json = mock.deferred_reply();
    let content = json["content"].as_str().expect("list content");
    assert!(content.contains("feed-own"), "own relay listed: {content}");
    assert!(
        !content.contains("feed-foreign"),
        "other guild's relay never listed: {content}"
    );
    assert!(
        db.feed_audits().await.is_empty(),
        "list writes no audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_add_plan_errors_write_nothing() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    // Unknown kind → InvalidKind; non-HTTPS source → SSRF guard refusal.
    for (kind, source) in [
        ("atom", "https://example.com/feed.xml"),
        ("rss", "http://example.com/feed.xml"),
    ] {
        runtime
            .on_interaction(&slash(
                "feed-add",
                Some(CHANNEL),
                vec![
                    option("kind", CommandOptionValue::String(kind.to_owned())),
                    option("source", CommandOptionValue::String(source.to_owned())),
                ],
            ))
            .await;
    }

    assert!(db.feed_rows().await.is_empty(), "no rows on plan errors");
    assert!(
        db.feed_audits().await.is_empty(),
        "no audit on plan errors (legacy parity)"
    );
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.method == "PATCH")
            .count(),
        2,
        "both errors still complete the defer"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_store_failure_replies_generic_and_audits_nothing() {
    let db = TestDb::new().await;
    // Drop the table after migrations: every store call fails. CASCADE covers
    // the feed_deliveries.feed_id foreign key (0180_feeds.sql).
    sqlx::query("DROP TABLE feed_relays CASCADE")
        .execute(&db.pool)
        .await
        .expect("drop feed_relays");
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash(
            "feed-add",
            Some(CHANNEL),
            vec![
                option("kind", CommandOptionValue::String("rss".to_owned())),
                option(
                    "source",
                    CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
                ),
            ],
        ))
        .await;

    assert_eq!(
        mock.deferred_reply()["content"],
        "Feed command failed; try again."
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_failed_defer_leaves_store_untouched() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, vec![404]).await;

    runtime
        .on_interaction(&slash(
            "feed-add",
            Some(CHANNEL),
            vec![
                option("kind", CommandOptionValue::String("rss".to_owned())),
                option(
                    "source",
                    CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
                ),
            ],
        ))
        .await;

    assert!(
        db.feed_rows().await.is_empty(),
        "no mutation when the defer fails"
    );
    assert!(db.feed_audits().await.is_empty());
    assert_eq!(
        mock.requests().len(),
        1,
        "the failed defer is the only call"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_and_feed_slices_share_one_runtime() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash("feed-list", Some(CHANNEL), Vec::new()))
        .await;
    runtime
        .on_interaction(&slash("sticky-remove", Some(CHANNEL), Vec::new()))
        .await;

    let edits: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH")
        .collect();
    assert_eq!(edits.len(), 2, "both slices complete through one executor");
    let feed_reply: serde_json::Value = serde_json::from_slice(&edits[0].body).unwrap();
    let sticky_reply: serde_json::Value = serde_json::from_slice(&edits[1].body).unwrap();
    assert_eq!(feed_reply["content"], "No feed relays configured.");
    assert_eq!(sticky_reply["content"], "No sticky in this channel.");
    mock.shutdown().await;
    db.close().await;
}
