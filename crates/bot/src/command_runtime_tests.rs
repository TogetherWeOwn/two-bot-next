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
//!    claim collision, debounce hold, REST-failure cleanup, the feed slice's
//!    CRUD + `announcements_audit_log` journeys, and the schedule slice's
//!    CRUD + `automation_audit_log` journeys. These run in CI via
//!    `TWO_GATEWAY_TEST_DATABASE_URL` (same approved service as the gateway
//!    suite — never the runtime DATABASE_URL).

#![cfg(test)]

#[path = "feed_sticky_strict_decode_acceptance_tests.rs"]
mod feed_sticky_strict_decode_acceptance_tests;
#[path = "reopen_dispatch_review_acceptance_tests.rs"]
mod reopen_dispatch_review_acceptance_tests;

#[path = "leveling_ack_ordering_acceptance_tests.rs"]
mod leveling_ack_ordering_acceptance_tests;
#[path = "leveling_ack_visibility_acceptance_tests.rs"]
mod leveling_ack_visibility_acceptance_tests;
#[path = "member_authority_cause_acceptance.rs"]
mod member_authority_cause_acceptance;
#[path = "member_dispatch_authority_acceptance.rs"]
mod member_dispatch_authority_acceptance;
#[path = "member_strict_decode_acceptance.rs"]
mod member_strict_decode_acceptance;
#[path = "sticky_uncertain_acceptance_tests.rs"]
mod sticky_uncertain_acceptance_tests;

#[path = "custom_command_list_pages_acceptance.rs"]
mod custom_command_list_pages_acceptance;
#[path = "custom_command_toggle_acceptance.rs"]
mod custom_command_toggle_acceptance;
#[path = "help_detail_acceptance.rs"]
mod help_detail_acceptance;

#[path = "game_mix_acceptance_tests.rs"]
mod game_mix_acceptance_tests;
#[path = "game_mix_content_acceptance_tests.rs"]
mod game_mix_content_acceptance_tests;
// Builder-owned: pins the classified store-failure warn the protected
// modules cannot cover without Test Author edits.
#[path = "game_mix_store_error_log_tests.rs"]
mod game_mix_store_error_log_tests;
#[path = "giveaway_admission_edge_acceptance_tests.rs"]
mod giveaway_admission_edge_acceptance_tests;
#[path = "giveaway_countdown_edge_acceptance_tests.rs"]
mod giveaway_countdown_edge_acceptance_tests;
#[path = "giveaway_create_entry_acceptance_tests.rs"]
mod giveaway_create_entry_acceptance_tests;
#[path = "schedule_absolute_acceptance_tests.rs"]
mod schedule_absolute_acceptance;
#[path = "schedule_absolute_correction_tests.rs"]
mod schedule_absolute_correction_tests;
#[path = "schedule_preview_dispatch_acceptance.rs"]
mod schedule_preview_dispatch_acceptance;
#[path = "schedule_strict_decode_acceptance_tests.rs"]
mod schedule_strict_decode_acceptance_tests;

use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use twilight_model::application::command::CommandType;
use twilight_model::application::interaction::application_command::{
    CommandData, CommandDataOption, CommandOptionValue,
};
use twilight_model::application::interaction::message_component::MessageComponentInteractionData;
use twilight_model::application::interaction::{Interaction, InteractionData, InteractionType};
use twilight_model::channel::message::component::ComponentType;
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
use two_bot_core::sticky::{store, ActivityOutcome, PutSticky};
use two_bot_core::{
    RouterGates, RouterRefusal, ANNOUNCEMENTS_DISABLED_REPLY, AUTOMATIONS_DISABLED_REPLY,
    MANAGE_EVENTS_REQUIRED, MANAGE_SERVER_REQUIRED, MODERATION_DISABLED_REPLY,
};
use two_bot_discord::ActionExecutor;

use crate::command_runtime::{
    actor_id, ephemeral, feed_add_options_strict, feed_id_option, feed_list_page_request,
    feed_remove_option_strict, help_command_selector, new_id, router_with_commands,
    sticky_options_strict, CommandRuntime, FeedListPage, RegistrySyncError,
};
use crate::internal_action_http::MODERATION_METRICS_LOCK;
use crate::member_runtime::{sweep_job, MemberRuntime};
use crate::privacy_command::{deletion_request_id, privacy_action, PrivacyAction};
use crate::schedule_runtime::{
    schedule_list_page_option_strict, schedule_options, schedule_options_strict,
    schedule_remove_option, schedule_remove_option_strict,
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
                        body: request.2.clone(),
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
                        } else if status == 200
                            && request.0 == "PUT"
                            && request.1.ends_with("/commands")
                        {
                            String::from_utf8(request.2).unwrap()
                        } else if status == 200
                            && (request.1.ends_with("/callback")
                                || request.1.ends_with("/messages/@original"))
                        {
                            "{\"id\":\"99\"}".to_owned()
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
        voice: false,
        voice_assistant: false,
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

pub(crate) fn message(id: u64, channel: u64, bot: bool, guild: Option<u64>) -> Message {
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
pub(crate) fn slash(
    name: &str,
    channel: Option<u64>,
    options: Vec<CommandDataOption>,
) -> Interaction {
    Interaction {
        app_permissions: None,
        application_id: Id::new(1111),
        authorizing_integration_owners: ApplicationIntegrationMap {
            guild: Some(AnonymizableId::Id(Id::new(GUILD))),
            user: None,
        },
        // Real Discord populates both for guild slash commands: the
        // registration guild (identity resolution scopes guild receipts by
        // it) and the channel object (template context needs it).
        channel: channel.map(|id| {
            serde_json::from_value::<twilight_model::channel::Channel>(serde_json::json!({
                "id": id.to_string(),
                "guild_id": GUILD.to_string(),
                "type": 0,
                "name": "commands",
            }))
            .expect("slash fixture channel")
        }),
        channel_id: channel.map(Id::new),
        context: None,
        data: Some(InteractionData::ApplicationCommand(Box::new(CommandData {
            guild_id: Some(Id::new(GUILD)),
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

fn lfg_select(values: Vec<String>) -> Interaction {
    let mut interaction = slash("lfg", Some(CHANNEL), Vec::new());
    interaction.kind = InteractionType::MessageComponent;
    interaction.data = Some(InteractionData::MessageComponent(Box::new(
        MessageComponentInteractionData {
            custom_id: two_bot_core::lfg::lfg_custom_id("lfg-test-post"),
            component_type: ComponentType::TextSelectMenu,
            resolved: None,
            values,
        },
    )));
    interaction.member.as_mut().unwrap().permissions = Some(Permissions::empty());
    interaction
}

/// The same interaction as `member` with `permissions`, with its own id/token.
fn from_member(mut interaction: Interaction, member: u64, permissions: Permissions) -> Interaction {
    interaction.id = Id::new(member);
    interaction.token = format!("token-{member}");
    let resolved = interaction.member.as_mut().expect("guild member");
    resolved.permissions = Some(permissions);
    resolved.user = Some(user(member, false));
    interaction
}

fn interaction_event(interaction: Interaction) -> Event {
    Event::InteractionCreate(Box::new(InteractionCreate(interaction)))
}

/// A router-refused command: the first thing it does is one REST callback, so
/// a delayed mock reply holds its worker slot without any database.
/// Move an interaction to another invocation guild. Real guild slash
/// payloads carry the same guild in `data.guild_id`, so keep both aligned.
fn set_guild(
    interaction: &mut Interaction,
    guild: Option<twilight_model::id::Id<twilight_model::id::marker::GuildMarker>>,
) {
    interaction.guild_id = guild;
    if let Some(InteractionData::ApplicationCommand(command)) = interaction.data.as_mut() {
        if command.guild_id.is_some() {
            command.guild_id = guild;
        }
    }
}

fn open_command(member: u64) -> Event {
    interaction_event(from_member(
        slash("sticky-remove", Some(CHANNEL), Vec::new()),
        member,
        Permissions::empty(),
    ))
}

/// `open_command` as a global-registration payload (no registration guild).
/// Scope-closure probes use it so a registry installed earlier in the test
/// cannot swallow the invocation before the closed scope is consulted.
fn open_command_global(member: u64) -> Event {
    let mut interaction = from_member(
        slash("sticky-remove", Some(CHANNEL), Vec::new()),
        member,
        Permissions::empty(),
    );
    if let Some(InteractionData::ApplicationCommand(command)) = interaction.data.as_mut() {
        command.guild_id = None;
    }
    interaction_event(interaction)
}

/// `/timeout` from a member who holds `MODERATE_MEMBERS`; moderation is gated
/// off in `gates`, so the router answers with one refusal callback.
fn moderator_command(member: u64) -> Event {
    interaction_event(from_member(
        slash("timeout", Some(CHANNEL), Vec::new()),
        member,
        Permissions::MODERATE_MEMBERS,
    ))
}

fn held_callbacks(count: usize) -> Vec<RestResponse> {
    (0..count)
        .map(|_| RestResponse {
            status: 200,
            body: None,
            delay: Duration::from_secs(30),
        })
        .collect()
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
    wait_for_timeout(predicate, what, Duration::from_secs(10)).await;
}

/// `wait_for` with an explicit deadline, for lane tests whose holders must
/// outlive slow CI schedulers. Poll-based, so a generous deadline costs
/// nothing when the lane drains fast.
async fn wait_for_timeout<F: Fn() -> bool>(predicate: F, what: &str, timeout: Duration) {
    tokio::time::timeout(timeout, async {
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
    CommandRuntime::from_gates(
        pool,
        executor,
        requested,
        None,
        None,
        None,
        two_bot_core::OnboardingGates {
            mode: two_bot_core::OnboardingMode::Legacy,
            dry_run: false,
        },
        &activation,
    )
}

#[tokio::test]
async fn activation_boot_publishes_only_permitted_capabilities() {
    use two_bot_core::{
        activation::LiveCapability, announcement_commands, automation_commands,
        moderation_commands, ComponentHandler, ComponentOutcome, HandlerId, SlashContext,
        SlashOutcome,
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
        let prefixed = token.starts_with("Bot ");
        assert_eq!(
            names.contains(&"lfg-list"),
            staging,
            "lfg-list publication, guild={guild}, app={app}, prefixed={prefixed}"
        );
        for handler in [
            HandlerId::AutomationAdmin,
            HandlerId::Giveaway,
            HandlerId::FeedAdd,
            HandlerId::FeedRemove,
            HandlerId::FeedPause,
            HandlerId::FeedResume,
            HandlerId::FeedList,
            HandlerId::FeedInspect,
        ] {
            assert_eq!(
                runtime.router().handler_for(&handler).is_some(),
                staging,
                "conditional {handler:?} registration, guild={guild}, app={app}, prefixed={prefixed}"
            );
        }
        // LFG adapter ownership is composed unconditionally, unlike the
        // activation-fenced publication and invocation checked separately.
        for handler in [
            HandlerId::Lfg,
            HandlerId::LfgClose,
            HandlerId::LfgEdit,
            HandlerId::LfgList,
        ] {
            assert_eq!(
                runtime.router().handler_for(&handler).map(|h| h.id()),
                Some(handler),
                "LFG ownership {handler:?}, guild={guild}, app={app}, prefixed={prefixed}"
            );
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
        for interaction_guild in [Some(guild), Some(9999), None] {
            for permissions in [
                Some(Permissions::MANAGE_EVENTS),
                Some(Permissions::MANAGE_GUILD),
                Some(Permissions::empty()),
                None,
            ] {
                let expected = if interaction_guild != Some(guild) {
                    SlashOutcome::Ignore
                } else if !staging {
                    SlashOutcome::Refuse {
                        refusal: RouterRefusal::AnnouncementsDisabled,
                    }
                } else if permissions == Some(Permissions::MANAGE_EVENTS) {
                    SlashOutcome::Handled {
                        handler: HandlerId::LfgList,
                    }
                } else {
                    SlashOutcome::Refuse {
                        refusal: RouterRefusal::ManageEventsRequired,
                    }
                };
                let context = format!(
                    "lfg-list guild={guild}, app={app}, prefixed={prefixed}, \
                     interaction_guild={interaction_guild:?}, permissions={permissions:?}"
                );
                assert_eq!(
                    runtime.router().route_slash(&SlashContext {
                        name: "lfg-list",
                        guild_id: interaction_guild,
                        actor_permissions: permissions.map(|p| p.bits()),
                        custom_row: None,
                    }),
                    expected,
                    "{context}"
                );
                if matches!(expected, SlashOutcome::Handled { .. }) {
                    // Positive routing control only: do not execute the DB read.
                    continue;
                }
                let mut interaction = slash("lfg-list", Some(CHANNEL), Vec::new());
                interaction.application_id = Id::new(app);
                // Global-registration payload: an unpublished command has no
                // guild registration to resolve, so the router must see it.
                interaction.guild_id = interaction_guild.map(Id::new);
                if let Some(InteractionData::ApplicationCommand(command)) =
                    interaction.data.as_mut()
                {
                    command.guild_id = None;
                }
                interaction.member.as_mut().unwrap().permissions = permissions;
                let before = mock.requests().len();
                tokio::time::timeout(Duration::from_secs(2), runtime.on_interaction(&interaction))
                    .await
                    .unwrap_or_else(|_| panic!("refusal must precede DB execution: {context}"));
                let requests = mock.requests();
                match expected {
                    SlashOutcome::Ignore => {
                        assert_eq!(requests.len(), before, "silent guild fence: {context}");
                    }
                    SlashOutcome::Refuse { refusal } => {
                        assert_eq!(
                            requests.len(),
                            before + 1,
                            "one refusal only; no lookup, defer, edit or effects: {context}"
                        );
                        let callback = &requests[before];
                        assert_eq!(callback.method, "POST", "{context}");
                        assert_eq!(
                            callback.path, "/api/v10/interactions/7/sticky-test-token/callback",
                            "{context}"
                        );
                        let reply: serde_json::Value =
                            serde_json::from_slice(&callback.body).unwrap();
                        assert_eq!(reply["type"], 4, "immediate refusal: {context}");
                        assert_eq!(reply["data"]["flags"], 64, "ephemeral: {context}");
                        assert_eq!(reply["data"]["content"], refusal.message(), "{context}");
                    }
                    other => panic!("unexpected negative control {other:?}: {context}"),
                }
            }
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
                vec![
                    "rank",
                    "leaderboard",
                    "help",
                    "privacy",
                    "reopen",
                    "welcome-preview",
                    "game-mix"
                ],
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
    let runtime = CommandRuntime::from_env(
        pool,
        token,
        guild,
        None,
        two_bot_core::OnboardingGates {
            mode: two_bot_core::OnboardingMode::Legacy,
            dry_run: false,
        },
        None,
        &activation,
        false,
    );
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
        assert_eq!(
            names,
            [
                "rank",
                "leaderboard",
                "help",
                "privacy",
                "reopen",
                "welcome-preview",
                "game-mix"
            ]
        );
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

/// Process-isolated fixture evidence for the two `run()` call-site guards the
/// port record flags as fixture-only: the `SelfRoles` match arm gating
/// `SelfRoleService::from_env` on `(gateway_prerequisites, store, permitted)`
/// and the `Automod`-permitted fence before `automod_gateway::resolve`. Each
/// case runs in an isolated child process with a cleared environment,
/// following `activation_boot_from_env_isolates_denied_moderation_validation`.
/// The fixture calls the real `crate::gateway_prerequisites` plus the
/// permission fence and constructors, and pins the exact
/// `live activation refused` log line plus narrow-not-throw per call site. It
/// does not execute `run()`, so a deleted or inverted guard in `run()` would
/// not fail this test; it pins the composition behavior the guards implement.
/// No boot/intent/gateway semantics change.
#[test]
fn activation_boot_call_sites_narrow_refused_guards() {
    const VALID_PANELS: &str = r#"[{"id":"games","channelId":"10000000000000007","messageId":"10000000000000008","mode":"button","options":[{"key":"chess","label":"Chess","roleId":"10000000000000004","permissions":"0"}]}]"#;
    for (guild, token, owen, protected_roles, panels, expected) in [
        // Staging pair with a valid catalogue: both guards permit, both
        // construct as today.
        (
            "1545644954272137297",
            "MTQ2OTEzNzYzNjY2Mzc1ODg4OA.mock.signature",
            "123456789012345678",
            "",
            VALID_PANELS,
            "permitted-constructs",
        ),
        // Live pair: the reviewed self_roles clearance stays while automod
        // refuses and never resolves, even with vars that would fail
        // validation once permitted.
        (
            "326474832151838730",
            "MTUzOTcxMTY4Mzg5ODExODE1NA.mock.signature",
            "",
            "invalid",
            VALID_PANELS,
            "live-automod-refused",
        ),
        // Unknown guild: both refuse; invalid vars prove narrow-not-throw.
        (
            "1555555555555555555",
            "MTQ2OTEzNzYzNjY2Mzc1ODg4OA.mock.signature",
            "",
            "invalid",
            "not-json",
            "both-refused",
        ),
        // Control: the same invalid vars under a permitted identity fail
        // loudly, so the refused narrow comes from the guard, not the vars.
        (
            "1545644954272137297",
            "MTQ2OTEzNzYzNjY2Mzc1ODg4OA.mock.signature",
            "",
            "invalid",
            "not-json",
            "permitted-validates",
        ),
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "command_runtime_tests::activation_boot_call_site_fixture",
                "--nocapture",
            ])
            .env_clear()
            .env("ACTIVATION_CALL_SITE_EXPECTED", expected)
            .env("GUILD_ID", guild)
            .env("DISCORD_TOKEN", token)
            .env(
                "DATABASE_URL",
                "postgres://agent_test@agent-testdb:5432/agent_test",
            )
            .env("DISCORD_API_BASE", "http://127.0.0.1:9")
            .env("TWO_MODERATION", "1")
            .env("TWO_OWEN_USER_ID", owen)
            .env("TWO_MODERATION_PROTECTED_ROLE_IDS", protected_roles)
            .env("TWO_AUTOMOD", "1")
            .env("TWO_SELF_ROLE_PANELS", panels)
            .env("TWO_SELF_ROLE_DRY_RUN", "0")
            .output()
            .expect("isolated call-site fixture");
        assert!(
            output.status.success(),
            "fixture {expected}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("1 passed"));
        // The marker goes to stderr (`print_stdout` is denied workspace-wide):
        // its presence proves the child ran this case's arm instead of
        // early-returning with the env var unset.
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("CALL_SITE") && stderr.contains(expected),
            "fixture {expected} must emit its summary line"
        );
    }
}

#[tokio::test]
async fn activation_boot_call_site_fixture() {
    let Ok(expected) = std::env::var("ACTIVATION_CALL_SITE_EXPECTED") else {
        return;
    };
    // Capture the boot refusal lines: `log_refusals` is the only writer in
    // this process, so the buffer holds exactly this identity's refusals.
    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let config = two_bot_core::Config::from_env().unwrap();
    let guild = config.guild_id.unwrap();
    let activation = crate::activation::BootActivation::from_config(&config);
    activation.log_refusals();
    let log = String::from_utf8(capture.0.lock().expect("capture lock").clone()).unwrap();

    let vars: std::collections::HashMap<String, String> = std::env::vars().collect();
    // Mirror the `SelfRoles` match arm in `run()`: the real
    // `crate::gateway_prerequisites` plus a present store gate the permission
    // check, and only the permitted path reaches catalogue parsing, the first
    // fallible step of `SelfRoleService::from_env`. No pool is built here; the
    // fixture stops at parsing, so the store is treated as present and the
    // permission fence is the varying input.
    let prerequisites_ok = crate::gateway_prerequisites(&config).is_ok();
    let self_permitted = activation.permitted(two_bot_core::activation::LiveCapability::SelfRoles);
    let automod_permitted = activation.permitted(two_bot_core::activation::LiveCapability::Automod);
    let mut self_constructed = false;
    let self_gates = if prerequisites_ok && self_permitted {
        self_constructed = true;
        Some(two_bot_core::self_roles::SelfRoleGates::from_map(&vars))
    } else {
        None
    };
    // Mirror the `Automod`-permitted fence in `run()`: a refused automod
    // capability is never resolved or validated; the refusal was already
    // logged once above.
    let mut automod_resolved = false;
    let automod_config = if automod_permitted {
        automod_resolved = true;
        Some(crate::automod_gateway::resolve(&vars, guild))
    } else {
        None
    };

    match expected.as_str() {
        "permitted-constructs" => {
            assert!(self_permitted && automod_permitted);
            assert!(
                !log.contains("live activation refused"),
                "staging permits every capability, so nothing is refused"
            );
            let gates = self_gates.expect("permitted self-roles reaches parsing");
            let gates = gates.expect("valid panels parse");
            assert!(!gates.panels.is_empty());
            let resolved = automod_config.expect("permitted automod reaches resolve");
            assert!(
                resolved.expect("valid automod resolves").is_some(),
                "permitted automod constructs as today"
            );
        }
        "live-automod-refused" => {
            assert!(
                self_permitted,
                "live pair keeps the reviewed self_roles clearance"
            );
            assert!(!automod_permitted);
            assert!(log.contains("live activation refused"));
            assert!(log.contains("automod"));
            assert!(
                !log.contains("self_roles"),
                "permitted self_roles logs no refusal"
            );
            let gates = self_gates.expect("permitted self-roles reaches parsing");
            assert!(
                !gates.expect("valid panels parse").panels.is_empty(),
                "permitted self-roles constructs as today"
            );
            assert!(
                !automod_resolved,
                "refused automod never resolves or validates"
            );
            assert!(
                automod_config.is_none(),
                "refused automod narrows, not throws"
            );
        }
        "both-refused" => {
            assert!(!self_permitted && !automod_permitted);
            assert!(log.contains("live activation refused"));
            assert!(log.contains("self_roles"));
            assert!(log.contains("automod"));
            assert!(
                !self_constructed,
                "refused SelfRoles composes no SelfRoleService"
            );
            assert!(self_gates.is_none());
            assert!(
                !automod_resolved,
                "refused Automod never resolves or validates"
            );
            assert!(
                automod_config.is_none(),
                "refused automod narrows, not throws"
            );
        }
        "permitted-validates" => {
            assert!(self_permitted && automod_permitted);
            assert!(
                self_gates
                    .expect("permitted self-roles reaches parsing")
                    .is_err(),
                "invalid panels fail once permitted"
            );
            assert!(
                automod_config
                    .expect("permitted automod reaches resolve")
                    .is_err(),
                "invalid automod vars fail once permitted"
            );
        }
        other => panic!("unknown call-site expectation {other}"),
    }
    // `eprintln!`: `clippy::print_stdout` is denied workspace-wide, and the
    // parent reads this marker from the child's stderr.
    eprintln!("CALL_SITE {expected} self_permitted={self_permitted} automod_permitted={automod_permitted}");
}

#[test]
fn sticky_options_strict_decodes_valid_and_ignores_unknown_names() {
    let full = slash(
        "sticky",
        Some(CHANNEL),
        vec![
            option("body", CommandOptionValue::String("remember".to_owned())),
            option("debounce", CommandOptionValue::Integer(7)),
            option("other", CommandOptionValue::Boolean(true)),
        ],
    );
    assert_eq!(
        sticky_options_strict(&full),
        Ok(("remember".to_owned(), Some(7)))
    );
    let omitted_debounce = slash(
        "sticky",
        Some(CHANNEL),
        vec![option(
            "body",
            CommandOptionValue::String("remember".to_owned()),
        )],
    );
    assert_eq!(
        sticky_options_strict(&omitted_debounce),
        Ok(("remember".to_owned(), None))
    );
}

#[test]
fn sticky_options_strict_refuses_wrong_types_duplicates_and_missing_body() {
    let wrong_body = slash(
        "sticky",
        Some(CHANNEL),
        vec![option("body", CommandOptionValue::Integer(9))],
    );
    assert_eq!(
        sticky_options_strict(&wrong_body),
        Err("Invalid body. Use /sticky body:<text> debounce:<seconds>.")
    );
    let wrong_debounce = slash(
        "sticky",
        Some(CHANNEL),
        vec![
            option("body", CommandOptionValue::String("pin me".to_owned())),
            option("debounce", CommandOptionValue::String("soon".to_owned())),
        ],
    );
    assert_eq!(
        sticky_options_strict(&wrong_debounce),
        Err("Invalid debounce. Use /sticky body:<text> debounce:<seconds>.")
    );
    let duplicate_body = slash(
        "sticky",
        Some(CHANNEL),
        vec![
            option("body", CommandOptionValue::String("pin me".to_owned())),
            option("body", CommandOptionValue::String("pin me".to_owned())),
        ],
    );
    assert_eq!(
        sticky_options_strict(&duplicate_body),
        Err("Duplicate body. Use /sticky body:<text> debounce:<seconds>.")
    );
    let duplicate_debounce = slash(
        "sticky",
        Some(CHANNEL),
        vec![
            option("body", CommandOptionValue::String("pin me".to_owned())),
            option("debounce", CommandOptionValue::Integer(7)),
            option("debounce", CommandOptionValue::Integer(7)),
        ],
    );
    assert_eq!(
        sticky_options_strict(&duplicate_debounce),
        Err("Duplicate debounce. Use /sticky body:<text> debounce:<seconds>.")
    );
    let missing_body = slash(
        "sticky",
        Some(CHANNEL),
        vec![option("debounce", CommandOptionValue::Integer(7))],
    );
    assert_eq!(
        sticky_options_strict(&missing_body),
        Err("Missing body. Use /sticky body:<text> debounce:<seconds>.")
    );
    // Duplicates win over wrong types, and `body` wins over `debounce`.
    let duplicate_wins = slash(
        "sticky",
        Some(CHANNEL),
        vec![
            option("body", CommandOptionValue::String("pin me".to_owned())),
            option("body", CommandOptionValue::Integer(9)),
            option("debounce", CommandOptionValue::String("soon".to_owned())),
        ],
    );
    assert_eq!(
        sticky_options_strict(&duplicate_wins),
        Err("Duplicate body. Use /sticky body:<text> debounce:<seconds>.")
    );
    // Wrong types beat a missing `body`: the unusable `debounce` is named
    // even though `body` is absent.
    let wrong_type_beats_missing = slash(
        "sticky",
        Some(CHANNEL),
        vec![option(
            "debounce",
            CommandOptionValue::String("soon".to_owned()),
        )],
    );
    assert_eq!(
        sticky_options_strict(&wrong_type_beats_missing),
        Err("Invalid debounce. Use /sticky body:<text> debounce:<seconds>.")
    );
    let mut no_data = slash("sticky", Some(CHANNEL), Vec::new());
    no_data.data = None;
    assert_eq!(
        sticky_options_strict(&no_data),
        Err("Missing body. Use /sticky body:<text> debounce:<seconds>.")
    );
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
        let runtime = runtime_without_db(gates(automations, false), automations, origin.clone());
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
async fn unconfirmed_help_does_not_invent_a_live_registry() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    assert!(runtime.published_commands().is_none());
    runtime
        .on_interaction(&slash("help", Some(CHANNEL), Vec::new()))
        .await;
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 1);
    let reply: serde_json::Value = serde_json::from_slice(&callbacks[0].body).unwrap();
    assert_eq!(reply["type"], 4);
    assert_eq!(reply["data"]["flags"], 64);
    assert_eq!(
        reply["data"]["content"],
        "The command list is still refreshing. Try /help again shortly."
    );
    assert_eq!(
        mock.requests().len(),
        1,
        "no publication or store work from help"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn help_answers_immediately_from_the_live_publish_set() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    // All feature gates on: the reply must equal the rendered live set.
    let router_gates = RouterGates {
        scorecard: true,
        moderation: true,
        ..gates(true, true)
    };
    let runtime = runtime_without_db(router_gates, true, origin);
    assert!(runtime.published_commands().is_none());
    runtime.publish_registry_checked(Some(1111)).await.unwrap();
    assert!(runtime.published_commands().is_some());
    runtime
        .on_interaction(&slash("help", Some(CHANNEL), Vec::new()))
        .await;
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 1, "one immediate callback, no defer");
    let reply: serde_json::Value =
        serde_json::from_slice(&callbacks[0].body).expect("callback json");
    assert_eq!(reply["type"], 4, "immediate response, not a defer");
    assert_eq!(reply["data"]["flags"], 64, "ephemeral");
    let content = reply["data"]["content"].as_str().expect("content");
    let defs = router_with_commands(router_gates)
        .publish_set(&[])
        .expect("live set");
    assert_eq!(content, two_bot_core::help_text(&defs));
    assert!(content.contains("/help"), "lists itself");
    assert!(content.contains("/ban"), "lists gated commands");
    assert!(
        content.contains("needs Ban Members"),
        "marks the gate: {content}"
    );
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
                    "sticky"
                        | "sticky-remove"
                        | "feed-add"
                        | "feed-remove"
                        | "feed-pause"
                        | "feed-resume"
                        | "feed-list"
                        | "feed-inspect"
                        | "purge"
                        | "purge-preview"
                        | "channel-inspect"
                        | "slowmode"
                        | "lockdown"
                        | "unlock"
                        | "lfg"
                        | "lfg-close"
                        | "lfg-edit"
                        | "lfg-list"
                        | "schedule"
                        | "schedule-remove"
                        | "schedule-list"
                        | "rank"
                        | "leaderboard"
                        | "help"
                        | "privacy"
                        | "giveaway"
                        | "reopen"
                        | "welcome-preview"
                        | "game-mix"
                )
            })
            .collect();
        assert!(!unwired.contains(&"giveaway"));
        assert!(!unwired.contains(&"rank"));
        assert!(!unwired.contains(&"leaderboard"));
        assert!(!unwired.contains(&"help"));
        assert!(!unwired.contains(&"privacy"));
        assert!(!unwired.contains(&"lfg-list"));
        assert!(!unwired.contains(&"reopen"));
        assert!(!unwired.contains(&"welcome-preview"));
        assert!(!unwired.contains(&"game-mix"));
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

// ---------------------------------------------------------------------------
// Member-moderation router-to-executor wiring (member runtime slice)
// ---------------------------------------------------------------------------

/// Runtime with the member-moderation consumer attached over a never-used
/// lazy pool — for the routing paths that return before any DB work.
fn runtime_with_member(
    router_gates: RouterGates,
    origin: String,
    pool: PgPool,
) -> Arc<CommandRuntime> {
    let automations = router_gates.automations;
    let executor =
        ActionExecutor::with_proxy("test-token".to_owned(), Some(origin)).expect("mock executor");
    let member = MemberRuntime::for_test(pool.clone(), GUILD_S);
    CommandRuntime::new_with_member(
        pool,
        executor,
        router_with_commands(router_gates),
        GUILD,
        automations,
        member,
    )
}

fn member_gates() -> RouterGates {
    RouterGates {
        moderation: true,
        ..gates(true, true)
    }
}

fn lazy_pool() -> PgPool {
    PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool")
}

/// Member verb with full actor permission and a target + reason, as the
/// router hands it to the member consumer.
fn member_slash(name: &str) -> Interaction {
    let mut interaction = slash(
        name,
        Some(CHANNEL),
        vec![
            option("target", CommandOptionValue::User(Id::new(3333))),
            option("reason", CommandOptionValue::String("wiring".to_owned())),
        ],
    );
    interaction.member.as_mut().unwrap().permissions = Some(Permissions::all());
    interaction
}

async fn member_refusal_database() -> two_bot_testsupport::TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap");
    two_bot_testsupport::TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("disposable member refusal fixture")
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb for refusal audits"]
async fn member_verbs_reach_executor_facts_then_fail_closed_with_audit() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = member_refusal_database().await;
    // Identity resolution fails before any authority/mutation. The failure
    // must still persist an attributable denial, so this now needs a ledger.
    for name in ["ban", "tempban", "kick", "timeout", "warn"] {
        let (mock, origin) = MockRest::start(vec![200, 500, 200]).await;
        let runtime = runtime_with_member(member_gates(), origin, db.pool().clone());
        runtime.on_interaction(&timed_member_slash(name)).await;
        let requests = mock.requests();
        assert_eq!(requests.len(), 3, "{name}: defer, guild read, reply edit");
        assert_eq!(requests[1].method, "GET");
        assert_eq!(requests[1].path, "/api/v10/users/@me");
        let reply = mock.deferred_reply();
        assert!(
            reply
                .to_string()
                .contains("Moderation command failed; try again."),
            "{name}: fail-closed reply, got {reply}"
        );
        mock.shutdown().await;
    }
    let denied: i64 =
        sqlx::query_scalar("SELECT count(*) FROM moderation_audit WHERE outcome = 'denied'")
            .fetch_one(db.pool())
            .await
            .expect("refusal audit");
    assert_eq!(denied, 5);
    db.close().await.expect("dispose fixture");
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb for refusal audits"]
async fn member_verbs_refuse_unknown_bot_hierarchy_before_claim_or_mutation() {
    // Keep process-global outcome assertions isolated from website tests.
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = member_refusal_database().await;
    for name in ["ban", "tempban", "kick", "timeout", "warn"] {
        let json_response = |body: serde_json::Value| RestResponse {
            status: 200,
            body: Some(body.to_string()),
            delay: Duration::ZERO,
        };
        let (mock, origin) = MockRest::start_script(vec![
            RestResponse::status(200),
            json_response(serde_json::to_value(user(4444, true)).unwrap()),
            json_response(serde_json::json!({
                "id": GUILD_S, "owner_id": "5555",
                "roles": [
                    {"id": GUILD_S, "position": 0, "permissions": "0"},
                    {"id": "101", "position": 50, "permissions": Permissions::all().bits().to_string()},
                ],
            })),
            json_response(serde_json::json!({"user": {"id": "88", "bot": false}, "roles": ["101"]})),
            json_response(serde_json::json!({"user": {"id": "3333", "bot": false}, "roles": []})),
            json_response(serde_json::json!({"user": {"id": "4444", "bot": true}, "roles": ["9999"]})),
            RestResponse::status(200),
        ])
        .await;
        let runtime = runtime_with_member(member_gates(), origin, db.pool().clone());
        runtime.on_interaction(&timed_member_slash(name)).await;
        let requests = mock.requests();
        assert_eq!(
            requests.len(),
            7,
            "{name}: facts and refusal audit only, no mutation or DB claim"
        );
        assert!(requests[1..6].iter().all(|request| request.method == "GET"));
        assert_eq!(
            requests[5].path,
            format!("/api/v10/guilds/{GUILD_S}/members/4444")
        );
        assert!(mock.deferred_reply()["content"]
            .as_str()
            .unwrap()
            .contains("Moderation command failed"));
        mock.shutdown().await;
    }
    let denied: i64 =
        sqlx::query_scalar("SELECT count(*) FROM moderation_audit WHERE outcome = 'denied'")
            .fetch_one(db.pool())
            .await
            .expect("refusal audit");
    assert_eq!(denied, 5);
    db.close().await.expect("dispose fixture");
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb for refusal audits"]
async fn member_verbs_share_actor_cooldown_before_role_reads() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = member_refusal_database().await;
    let (mock, origin) = MockRest::start(vec![200, 500, 200]).await;
    let runtime = runtime_with_member(member_gates(), origin, db.pool().clone());
    runtime.on_interaction(&member_slash("ban")).await;
    for (index, name) in ["tempban", "kick", "timeout", "warn"]
        .into_iter()
        .enumerate()
    {
        let mut interaction = timed_member_slash(name);
        interaction.id = Id::new(100 + index as u64);
        runtime.on_interaction(&interaction).await;
        let requests = mock.requests();
        let reply: serde_json::Value =
            serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        assert_eq!(
            reply["content"],
            "Wait 5 seconds before using another member moderation command."
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method == "GET")
                .count(),
            1
        );
    }
    mock.shutdown().await;
    db.close().await.expect("dispose fixture");
}

fn scripted_json(body: serde_json::Value) -> RestResponse {
    RestResponse {
        status: 200,
        body: Some(body.to_string()),
        delay: Duration::ZERO,
    }
}

/// The reads one member verb makes before it can act, in issue order: the bot
/// identity (cached after the first command), the guild, then the actor. The
/// actor holds every permission at position 50; the bot sits at 100.
fn authority_reads(first_command: bool) -> Vec<RestResponse> {
    let mut reads = Vec::new();
    if first_command {
        reads.push(scripted_json(
            serde_json::to_value(user(4444, true)).unwrap(),
        ));
    }
    reads.push(scripted_json(serde_json::json!({
        "id": GUILD_S, "owner_id": "5555",
        "roles": [
            {"id": GUILD_S, "position": 0, "permissions": "0"},
            {"id": "101", "position": 50, "permissions": Permissions::all().bits().to_string()},
            {"id": "103", "position": 100, "permissions": "0"},
        ],
    })));
    reads.push(scripted_json(
        serde_json::json!({"user": {"id": "88", "bot": false}, "roles": ["101"]}),
    ));
    reads
}

fn timed_member_slash(name: &str) -> Interaction {
    let mut interaction = member_slash(name);
    if matches!(name, "tempban" | "timeout") {
        let Some(InteractionData::ApplicationCommand(data)) = &mut interaction.data else {
            panic!("command fixture")
        };
        data.options
            .push(option("duration_seconds", CommandOptionValue::Integer(60)));
    }
    interaction
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb for refusal audits"]
async fn member_verbs_that_need_a_present_target_refuse_a_departed_one_without_effects() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = member_refusal_database().await;
    for name in ["kick", "timeout", "warn"] {
        let mut script = vec![RestResponse::status(200)];
        script.extend(authority_reads(true));
        // A strict target-member 404 is a confirmed departure.
        script.push(RestResponse::status(404));
        script.push(RestResponse::status(200));
        let (mock, origin) = MockRest::start_script(script).await;
        let runtime = runtime_with_member(member_gates(), origin, db.pool().clone());
        runtime.on_interaction(&timed_member_slash(name)).await;
        let requests = mock.requests();
        assert_eq!(
            requests.len(),
            6,
            "{name}: defer, bot, guild, actor, target, reply; no bot-member read or effect"
        );
        assert!(requests[1..5].iter().all(|request| request.method == "GET"));
        assert_eq!(
            requests[4].path,
            format!("/api/v10/guilds/{GUILD_S}/members/3333")
        );
        assert!(
            !requests
                .iter()
                .any(|r| r.method != "GET" && r.path.starts_with("/api/v10/guilds/")),
            "{name}: a departed target is never dispatched against"
        );
        assert_eq!(
            mock.deferred_reply()["content"],
            "That member is not in this server."
        );
        mock.shutdown().await;
    }
    let denied: Vec<(String, String)> =
        sqlx::query_as("SELECT action, outcome FROM moderation_audit ORDER BY action")
            .fetch_all(db.pool())
            .await
            .expect("refusal audit");
    assert_eq!(
        denied,
        ["moderation.kick", "moderation.timeout", "moderation.warn"]
            .map(|action| (action.to_owned(), "denied".to_owned())),
        "exactly one denial per verb and no accepted audit"
    );
    let claims: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_idempotency")
        .fetch_one(db.pool())
        .await
        .expect("claim rows");
    let warnings: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_warnings")
        .fetch_one(db.pool())
        .await
        .expect("warning rows");
    assert_eq!(
        (claims, warnings),
        (0, 0),
        "refusal leaves no claim or warning"
    );
    db.close().await.expect("dispose fixture");
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb for refusal audits"]
async fn a_completed_kick_replays_after_its_target_left_without_a_denial() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = member_refusal_database().await;
    let mut script = vec![RestResponse::status(200)];
    script.extend(authority_reads(true));
    script.push(scripted_json(serde_json::json!(
        {"user": {"id": "3333", "bot": false}, "roles": []}
    )));
    script.push(scripted_json(
        serde_json::json!({"user": {"id": "4444", "bot": true}, "roles": ["103"]}),
    ));
    script.push(RestResponse::status(204));
    script.push(RestResponse::status(200));
    // Redelivery of the same interaction: the kick removed the target, so its
    // member read is now a strict 404.
    script.push(RestResponse::status(200));
    script.extend(authority_reads(false));
    script.push(RestResponse::status(404));
    script.push(RestResponse::status(200));
    let (mock, origin) = MockRest::start_script(script).await;
    let runtime = runtime_with_member(member_gates(), origin, db.pool().clone());
    let interaction = member_slash("kick");
    runtime.on_interaction(&interaction).await;
    runtime.on_interaction(&interaction).await;
    let requests = mock.requests();
    let replies: Vec<String> = requests
        .iter()
        .filter(|r| r.method == "PATCH")
        .map(|r| {
            serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["content"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    // The fixture interaction carries no resolved user block, so the reply
    // slot falls back to the raw target user id.
    assert_eq!(replies, ["Kicked 3333.", "Kicked 3333. Already applied."]);
    let kicks: Vec<_> = requests
        .iter()
        .filter(|r| r.method == "DELETE" && r.path.starts_with("/api/v10/guilds/"))
        .collect();
    assert_eq!(kicks.len(), 1, "the replay dispatches nothing");
    let audits: Vec<(String, String)> =
        sqlx::query_as("SELECT action, outcome FROM moderation_audit")
            .fetch_all(db.pool())
            .await
            .expect("audit rows");
    assert_eq!(
        audits,
        [("moderation.kick".to_owned(), "kicked".to_owned())],
        "one acceptance and no replay denial"
    );
    let state: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT state, outcome FROM moderation_idempotency WHERE guild_id = $1 AND idempotency_key = $2",
    )
    .bind(GUILD_S)
    .bind(interaction.id.get().to_string())
    .fetch_optional(db.pool())
    .await
    .expect("replay ledger");
    assert_eq!(state, Some(("done".to_owned(), Some("kicked".to_owned()))));
    mock.shutdown().await;
    db.close().await.expect("dispose fixture");
}

#[tokio::test]
async fn member_verbs_refuse_when_moderation_gate_off() {
    // Gate-off keeps the verbs closed even with the consumer attached: one
    // immediate refusal, no defer, no other REST.
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_with_member(gates(true, true), origin, lazy_pool());
    runtime.on_interaction(&member_slash("ban")).await;
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 1, "one refusal callback");
    let reply: serde_json::Value = serde_json::from_slice(&callbacks[0].body).expect("reply json");
    assert_eq!(reply["type"], 4);
    assert_eq!(reply["data"]["content"], MODERATION_DISABLED_REPLY);
    assert_eq!(mock.requests().len(), 1, "no other REST effects");
    mock.shutdown().await;
}

#[tokio::test]
async fn member_ban_refuses_without_ban_permission() {
    // Moderation on, but the actor holds only MANAGE_GUILD: the shared router
    // refuses before any defer, DB or REST work.
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_with_member(member_gates(), origin, lazy_pool());
    let interaction = slash("ban", Some(CHANNEL), Vec::new());
    assert_eq!(
        interaction.member.as_ref().unwrap().permissions,
        Some(Permissions::MANAGE_GUILD),
        "fixture actor lacks BAN_MEMBERS"
    );
    runtime.on_interaction(&interaction).await;
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 1, "one refusal callback");
    let reply: serde_json::Value = serde_json::from_slice(&callbacks[0].body).expect("reply json");
    assert_eq!(reply["type"], 4);
    assert_eq!(
        reply["data"]["content"],
        "You need the Ban Members permission to use /ban. Ask a server moderator or admin to grant it."
    );
    assert_eq!(mock.requests().len(), 1, "no other REST effects");
    mock.shutdown().await;
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb for refusal audits"]
async fn rejected_ban_through_command_path_logs_detail_with_ids_and_replies_fixed_copy() {
    // The command path must wire execute() to error_text_with_log: the
    // moderator gets exactly the fixed copy while the server log keeps the
    // full detail with the interaction and request ids. Reverting execute()
    // to error_text keeps the reply byte-identical but drops the log line,
    // so this test reads the process-wide capture (fresh text only) with a
    // unique interaction id.
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = member_refusal_database().await;
    let capture = crate::tracing_capture::global();
    let log_start = capture.text().len();
    let mut script = vec![RestResponse::status(200)];
    script.extend(authority_reads(true));
    script.push(scripted_json(serde_json::json!(
        {"user": {"id": "3333", "bot": false}, "roles": []}
    )));
    script.push(scripted_json(
        serde_json::json!({"user": {"id": "4444", "bot": true}, "roles": ["103"]}),
    ));
    script.push(RestResponse::status(403));
    script.push(RestResponse::status(200));
    let (mock, origin) = MockRest::start_script(script).await;
    let runtime = runtime_with_member(member_gates(), origin, db.pool().clone());
    let mut interaction = member_slash("ban");
    interaction.id = Id::new(9007001);
    runtime.on_interaction(&interaction).await;
    let reply = mock.deferred_reply();
    assert_eq!(
        reply["content"],
        "Discord refused the request; nothing was applied. Check the bot's permissions or ask an operator."
    );
    assert!(
        !reply["content"]
            .as_str()
            .unwrap_or_default()
            .contains("403"),
        "rejection copy carries no status detail: {reply}"
    );
    let bans: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PUT" && r.path.contains("/bans/"))
        .collect();
    assert_eq!(bans.len(), 1, "one refused ban dispatch");
    let logged = capture.text().split_off(log_start);
    let line = logged
        .lines()
        .find(|line| {
            line.contains("member moderation discord failure")
                && line.contains("9007001")
                && line.contains("Discord refused the request with 403")
        })
        .unwrap_or_else(|| panic!("rejection must log detail with ids; fresh log: {logged:?}"));
    assert!(
        line.contains("request_id"),
        "log carries the request id: {line:?}"
    );
    mock.shutdown().await;
    db.close().await.expect("dispose fixture");
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb for refusal audits"]
async fn sweep_tick_reports_a_failed_unban_as_a_rest_job_failure() {
    // The sweep continues past per-job failures, but the tick must report
    // them: Ok would reset the member_unban_sweep consecutive-failure
    // streak and silence its failure-only alert while expired tempbans
    // never lift.
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = member_refusal_database().await;
    let mut script = vec![RestResponse::status(200)];
    script.extend(authority_reads(true));
    script.push(scripted_json(serde_json::json!(
        {"user": {"id": "3333", "bot": false}, "roles": []}
    )));
    script.push(scripted_json(
        serde_json::json!({"user": {"id": "4444", "bot": true}, "roles": ["103"]}),
    ));
    script.push(RestResponse::status(204));
    script.push(RestResponse::status(200));
    let (mock, origin) = MockRest::start_script(script).await;
    let runtime = runtime_with_member(member_gates(), origin, db.pool().clone());
    let mut interaction = member_slash("tempban");
    let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_mut() else {
        panic!("command fixture");
    };
    data.options.push(option(
        "duration_seconds",
        CommandOptionValue::Integer(3600),
    ));
    runtime.on_interaction(&interaction).await;
    assert_eq!(mock.deferred_reply()["content"], "Temporarily banned 3333.");
    // The successful PUT activates the row staged -> pending; the sweep
    // claims pending rows whose execute_at has passed.
    let scheduled: (String,) =
        sqlx::query_as("SELECT state FROM moderation_scheduled_unbans WHERE guild_id = $1")
            .bind(GUILD_S)
            .fetch_one(db.pool())
            .await
            .expect("tempban stages its unban");
    assert_eq!(scheduled.0, "pending");
    sqlx::query(
        "UPDATE moderation_scheduled_unbans SET execute_at = '2020-01-01T00:00:00+00:00' WHERE guild_id = $1",
    )
    .bind(GUILD_S)
    .execute(db.pool())
    .await
    .expect("backdate the staged job");
    mock.shutdown().await;

    // The DELETE is refused: the job requeues, the failure is audited, and
    // the tick reports Rest so the job streak records the failure.
    let (sweep_mock, sweep_origin) = MockRest::start_script(vec![
        scripted_json(serde_json::to_value(user(4444, true)).unwrap()),
        RestResponse::status(403),
    ])
    .await;
    let sweep_rest = ActionExecutor::with_proxy("test-token".to_owned(), Some(sweep_origin))
        .expect("mock executor");
    let member = MemberRuntime::for_test(db.pool().clone(), GUILD_S);
    let result = (sweep_job(member, sweep_rest).action)().await;
    assert!(
        matches!(result, Err(crate::jobs::ErrorClass::Rest)),
        "a failed sweep reports Rest, got {result:?}"
    );
    let failures: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM moderation_audit WHERE guild_id = $1 AND outcome = 'unban_failed'",
    )
    .bind(GUILD_S)
    .fetch_one(db.pool())
    .await
    .expect("failure audit");
    assert_eq!(failures.0, 1);
    assert_eq!(sweep_mock.deletes().len(), 1, "one refused DELETE");
    sweep_mock.shutdown().await;

    // Nothing due anywhere else stays a clean Ok: the failure signal fires
    // only when a job actually failed.
    let (idle_mock, idle_origin) = MockRest::start_script(vec![scripted_json(
        serde_json::to_value(user(4444, true)).unwrap(),
    )])
    .await;
    let idle_rest = ActionExecutor::with_proxy("test-token".to_owned(), Some(idle_origin))
        .expect("mock executor");
    let idle = MemberRuntime::for_test(db.pool().clone(), "9999");
    let idle_result = (sweep_job(idle, idle_rest).action)().await;
    assert!(
        matches!(idle_result, Ok(())),
        "an empty sweep succeeds, got {idle_result:?}"
    );
    idle_mock.shutdown().await;
    db.close().await.expect("dispose fixture");
}

#[tokio::test]
async fn unknown_command_replies_ephemerally_without_other_effects() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash("not-a-command", Some(CHANNEL), Vec::new()))
        .await;
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 1);
    let reply: serde_json::Value = serde_json::from_slice(&callbacks[0].body).unwrap();
    assert_eq!(reply["type"], 4, "immediate response, not a defer");
    assert_eq!(reply["data"]["flags"], 64);
    assert_eq!(
        reply["data"]["content"],
        two_bot_core::router::replies::UNKNOWN_COMMAND_REPLY
    );
    assert_eq!(
        reply["data"]["allowed_mentions"]["parse"],
        serde_json::json!([])
    );
    assert_eq!(
        mock.requests().len(),
        1,
        "only the unknown-command callback"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn foreign_non_moderation_commands_remain_silent() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    for name in ["not-a-command", "rank", "rsvp", "command", "schedule"] {
        for guild in [Some(Id::new(9999)), None] {
            let mut interaction = slash(name, Some(CHANNEL), Vec::new());
            set_guild(&mut interaction, guild);
            runtime.on_interaction(&interaction).await;
        }
    }
    assert!(mock.requests().is_empty(), "router Ignore stays silent");
    mock.shutdown().await;
}

/// `/reopen` routes `Handled` but executes in the onboarding worker, not
/// here: this runtime must stay silent (no unavailable reply, no defer),
/// exactly like it does for the picker components, so the single initial
/// callback belongs to the onboarding owner.
#[tokio::test]
async fn reopen_stays_silent_for_the_onboarding_owner() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash("reopen", Some(CHANNEL), Vec::new()))
        .await;
    assert!(
        mock.requests().is_empty(),
        "the onboarding worker owns /reopen; this runtime answers nothing"
    );
    mock.shutdown().await;
}

/// The inner leveling fence (legacy 7df2a95, `registerLeveling`): a foreign or
/// missing guild returns `Ok(false)` before the profile or leaderboard read.
/// The pool here is a never-used lazy connection to a closed port, so a read
/// moved ahead of the fence would fail this test instead of passing silently.
#[tokio::test]
async fn leveling_interaction_fence_precedes_any_store_read() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    let leveling = runtime.leveling();
    for (name, handler) in [
        ("rank", two_bot_core::HandlerId::Rank),
        ("leaderboard", two_bot_core::HandlerId::Leaderboard),
    ] {
        for guild in [Some(Id::new(9999)), None] {
            let mut interaction = slash(name, Some(CHANNEL), Vec::new());
            set_guild(&mut interaction, guild);
            let handled = leveling
                .handle_interaction(&interaction, handler)
                .await
                .expect("the guild fence returns before any store read");
            assert!(!handled, "{name} in guild {guild:?} is not handled");
        }
    }
    assert!(mock.requests().is_empty(), "no foreign-guild effects");
    mock.shutdown().await;
}

/// The 21 voice names that publish beside the builtins (`kick` is
/// moderation's under first-wins).
const VOICE_ONLY_NAMES: [&str; 21] = [
    "create",
    "setup",
    "ping",
    "invite",
    "textchannels",
    "access",
    "reclaim",
    "transfer",
    "logging",
    "export",
    "import",
    "position",
    "group",
    "inheritpermissions",
    "defaultlimit",
    "alwaysprivate",
    "name",
    "private",
    "public",
    "limit",
    "unlimit",
];

#[tokio::test]
async fn voice_commands_stay_silent_for_the_voice_sink_only_while_voice_is_live() {
    // Voice live: the sink answers every voice name, so the shared runtime
    // sends neither a callback nor any other REST effect.
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let live = RouterGates {
        voice: true,
        ..gates(true, true)
    };
    let runtime = runtime_without_db(live, true, origin);
    for name in VOICE_ONLY_NAMES {
        runtime
            .on_interaction(&slash(name, Some(CHANNEL), Vec::new()))
            .await;
    }
    assert!(
        mock.requests().is_empty(),
        "the router yields voice names to the sink"
    );
    mock.shutdown().await;

    // Voice off (or the sink failed to build): unchanged unknown-command reply.
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash("create", Some(CHANNEL), Vec::new()))
        .await;
    assert_eq!(mock.posts_to("/callback").await.len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn foreign_moderation_preserves_the_router_guild_refusal() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(false, false), false, origin);
    let mut interaction = slash("ban", Some(CHANNEL), Vec::new());
    set_guild(&mut interaction, Some(Id::new(9999)));
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

#[tokio::test]
async fn dispatch_remaining_keeps_rsvp_replies_and_registry_with_the_ordered_owner() {
    for announcements in [false, true] {
        let (mock, origin) = MockRest::start(Vec::new()).await;
        let runtime = runtime_without_db(gates(false, announcements), false, origin);
        runtime.dispatch_remaining(&ready());
        runtime.dispatch_remaining(&Event::Resumed);
        for name in ["rsvp", "rsvp-attendance", "attendance"] {
            for permissions in [Permissions::empty(), Permissions::all()] {
                let mut interaction = slash(name, Some(CHANNEL), Vec::new());
                interaction.member.as_mut().unwrap().permissions = Some(permissions);
                runtime.dispatch_remaining(&Event::InteractionCreate(Box::new(InteractionCreate(
                    interaction,
                ))));
            }
        }
        // Unowned sticky refusals must still use detached shared dispatch.
        runtime.dispatch_remaining(&Event::InteractionCreate(Box::new(InteractionCreate(
            slash("sticky-remove", Some(CHANNEL), Vec::new()),
        ))));
        wait_for(|| !mock.requests().is_empty(), "remaining sticky refusal").await;
        let requests = mock.requests();
        assert_eq!(
            requests.len(),
            1,
            "no RSVP callback or registry replacement"
        );
        let reply: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(reply["type"], 4);
        assert_eq!(reply["data"]["flags"], 64);
        assert_eq!(reply["data"]["content"], AUTOMATIONS_DISABLED_REPLY);
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn dispatch_remaining_ready_initializes_identity_without_registry_or_tickets() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(false, false), false, origin);
    runtime.dispatch_remaining(&ready());
    // The ordered surface owns publication; identity still initializes here so
    // LFG keeps READY's ids without a REST read.
    assert_eq!(runtime.test_interactions().test_bot_user_id(), 1111);
    assert_eq!(runtime.test_interactions().test_application_id(), 1111);
    assert_eq!(
        runtime
            .test_interactions()
            .test_resolved_bot_user_id()
            .await,
        1111
    );
    assert!(mock.requests().is_empty(), "no registry or identity HTTP");
    mock.shutdown().await;
}

#[tokio::test]
async fn dispatch_remaining_ready_mismatch_keeps_boot_pin_unarmed() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(false, false), false, origin);
    // Boot token pins the application identity on both the runtime and the
    // ordered surface; the bot user stays unknown until a matching READY.
    // `ready()` carries a foreign application (1111).
    runtime.set_identity(0, 2222);
    runtime.dispatch_remaining(&ready());
    // Mismatch: the old pin survives, the bot user never arms, and no
    // registry or store work runs under the foreign identity.
    assert_eq!(runtime.test_interactions().test_application_id(), 2222);
    assert_eq!(runtime.test_interactions().test_bot_user_id(), 0);
    assert!(mock.requests().is_empty(), "no registry or identity HTTP");
    // A READY matching the boot pin still initializes the bot user.
    let mut matching = ready();
    if let Event::Ready(ref mut ready) = matching {
        ready.application.id = Id::new(2222);
        ready.user.id = Id::new(2222);
    }
    runtime.dispatch_remaining(&matching);
    assert_eq!(runtime.test_interactions().test_application_id(), 2222);
    assert_eq!(runtime.test_interactions().test_bot_user_id(), 2222);
    assert!(mock.requests().is_empty(), "no registry or identity HTTP");
    mock.shutdown().await;
}

#[tokio::test]
async fn dispatch_remaining_ready_wakes_tickets_and_survives_failed_lookup() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    let executor =
        ActionExecutor::with_proxy("test-token".to_owned(), Some(origin)).expect("mock executor");
    let tickets = Arc::new(
        crate::ticket_runtime::TicketRuntime::new(
            pool.clone(),
            executor.clone(),
            crate::ticket_runtime::TicketConfig {
                guild_id: "100".into(),
                category_id: "200".into(),
                panel_channel_id: "700".into(),
                staff_role_id: "300".into(),
                cooldown_seconds: 15,
            },
            std::collections::HashMap::new(),
        )
        .unwrap(),
    );
    let runtime = CommandRuntime::with_tickets(pool, executor, Arc::clone(&tickets));
    runtime.dispatch_remaining(&ready());
    // Tickets wake from READY's bot id with no registry publication.
    assert_eq!(tickets.readiness_for_test(), (1111, 1));
    assert!(mock.requests().is_empty(), "no registry HTTP");
    // The mock is gone: any identity HTTP lookup would fail, but READY's
    // cached bot id is retained for LFG nonce recovery.
    mock.shutdown().await;
    assert_eq!(
        runtime
            .test_interactions()
            .test_resolved_bot_user_id()
            .await,
        1111
    );
}

#[tokio::test]
async fn bounded_dispatch_keeps_publication_independent_and_cancels_on_gateway_exit() {
    let (mock, origin) = MockRest::start_script(held_callbacks(17)).await;
    let runtime = runtime_without_db(gates(false, false), false, origin);
    let guard = runtime.dispatch_guard();
    for member in 1..=16 {
        assert!(runtime.dispatch(&open_command(member)));
    }
    for member in 17..117 {
        assert!(
            !runtime.dispatch(&open_command(member)),
            "no spawned waiters on saturation"
        );
    }
    assert!(
        runtime.dispatch(&ready()),
        "registry has separate admission"
    );
    assert!(
        !runtime.dispatch(&ready()),
        "overlapping registry sync coalesces"
    );
    wait_for(
        || {
            let requests = mock.requests();
            (1..=16).all(|member| {
                requests
                    .iter()
                    .any(|r| r.path.contains(&format!("/token-{member}/")))
            }) && requests.iter().any(|r| r.method == "PUT")
        },
        "all admitted work started",
    )
    .await;
    wait_for(
        || {
            mock.requests()
                .iter()
                .any(|r| r.path.contains("/token-17/"))
        },
        "a busy reply for the first refused member",
    )
    .await;
    drop(guard);
    wait_for(
        || Arc::strong_count(&runtime) == 1,
        "all scoped work cancelled",
    )
    .await;
    let sent = mock.requests().len();
    assert!(
        !runtime.dispatch(&open_command_global(500)),
        "closed scope refuses new work"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(mock.requests().len(), sent, "closed scope sent no HTTP");
    mock.shutdown().await;
}

fn callback_json(request: &RestRequest) -> serde_json::Value {
    serde_json::from_slice(&request.body).expect("callback body")
}

#[tokio::test]
async fn saturated_interaction_lane_still_answers_a_moderator_command() {
    // Sixteen members fill the shared interaction lane; each worker is held on
    // its first callback.
    let (mock, origin) = MockRest::start_script(held_callbacks(16)).await;
    let runtime = runtime_without_db(gates(false, false), false, origin);
    let _guard = runtime.dispatch_guard();
    for member in 1..=16 {
        assert!(runtime.dispatch(&open_command(member)));
    }
    assert!(
        !runtime.dispatch(&open_command(17)),
        "the shared lane is full for open commands"
    );

    assert!(
        runtime.dispatch(&moderator_command(900)),
        "the reserved lane admits a permitted moderator"
    );
    wait_for(
        || {
            mock.requests()
                .iter()
                .any(|r| r.path.contains("/token-900/"))
        },
        "moderator callback",
    )
    .await;
    let moderator = mock
        .requests()
        .into_iter()
        .find(|r| r.path.contains("/token-900/"))
        .expect("moderator callback");
    let reply = callback_json(&moderator);
    assert_eq!(reply["type"], 4);
    assert_eq!(
        reply["data"]["content"],
        RouterRefusal::ModerationDisabled.message(),
        "the router, not admission, answered the command"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn voice_owned_command_yields_before_per_member_admission() {
    let cap = crate::interaction_admission::PER_USER_IN_FLIGHT;
    let (mock, origin) = MockRest::start_script(held_callbacks(cap)).await;
    let router = RouterGates {
        voice: true,
        ..gates(false, false)
    };
    let runtime = runtime_without_db(router, false, origin);
    let _guard = runtime.dispatch_guard();
    for _ in 0..cap {
        assert!(runtime.dispatch(&open_command(42)));
    }
    wait_for(
        || {
            mock.requests()
                .iter()
                .filter(|request| request.path.contains("/token-42/"))
                .count()
                == cap
        },
        "per-member lane filled",
    )
    .await;

    let mut published = two_bot_discord::publish_commands(&[two_bot_core::CommandDefinition::new(
        "ping",
        "Synthetic voice ping.",
    )]);
    published[0].id = Some(Id::new(9001));
    runtime
        .executor()
        .command_identities()
        .replace_guild(1111, GUILD, &published)
        .unwrap();
    let mut invocation = slash("help", Some(CHANNEL), Vec::new());
    let Some(InteractionData::ApplicationCommand(command)) = invocation.data.as_mut() else {
        unreachable!();
    };
    command.id = Id::new(9001);
    command.guild_id = Some(Id::new(GUILD));
    let voice = interaction_event(from_member(invocation, 42, Permissions::empty()));
    assert!(runtime.dispatch(&voice), "the voice sink owns /ping");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|request| request.path.contains("/token-42/"))
            .count(),
        cap,
        "voice does not receive a competing busy callback"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn voice_owned_command_yields_before_shared_lane_saturation() {
    let (mock, origin) = MockRest::start_script(held_callbacks(16)).await;
    let router = RouterGates {
        voice: true,
        ..gates(false, false)
    };
    let runtime = runtime_without_db(router, false, origin);
    let _guard = runtime.dispatch_guard();
    for member in 1..=16 {
        assert!(runtime.dispatch(&open_command(member)));
    }
    wait_for(
        || {
            mock.requests()
                .iter()
                .filter(|request| request.path.contains("/callback"))
                .count()
                == 16
        },
        "shared interaction lane filled",
    )
    .await;

    let voice = interaction_event(from_member(
        slash("ping", Some(CHANNEL), Vec::new()),
        900,
        Permissions::empty(),
    ));
    assert!(runtime.dispatch(&voice), "the voice sink owns /ping");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        mock.requests()
            .iter()
            .all(|request| !request.path.contains("/token-900/")),
        "saturation must not send a busy callback for a voice command"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn refused_open_command_gets_one_ephemeral_busy_reply() {
    let (mock, origin) = MockRest::start_script(held_callbacks(16)).await;
    let runtime = runtime_without_db(gates(false, false), false, origin);
    let _guard = runtime.dispatch_guard();
    for member in 1..=16 {
        assert!(runtime.dispatch(&open_command(member)));
    }
    assert!(!runtime.dispatch(&open_command(17)));
    wait_for(
        || {
            mock.requests()
                .iter()
                .any(|r| r.path.contains("/token-17/"))
        },
        "busy callback",
    )
    .await;
    let busy: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.path.contains("/token-17/"))
        .collect();
    assert_eq!(busy.len(), 1, "one callback, no retry loop");
    let reply = callback_json(&busy[0]);
    assert_eq!(reply["type"], 4);
    assert_eq!(reply["data"]["flags"], 64, "ephemeral");
    assert_eq!(
        reply["data"]["content"],
        crate::interaction_admission::BUSY_REPLY
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn one_member_cannot_hold_more_than_their_share_of_the_lane() {
    let cap = crate::interaction_admission::PER_USER_IN_FLIGHT;
    let (mock, origin) = MockRest::start_script(held_callbacks(cap + 1)).await;
    let runtime = runtime_without_db(gates(false, false), false, origin);
    let _guard = runtime.dispatch_guard();
    for _ in 0..cap {
        assert!(runtime.dispatch(&open_command(42)));
    }
    assert!(
        !runtime.dispatch(&open_command(42)),
        "the member is at their cap while the lane has room"
    );
    assert!(
        runtime.dispatch(&open_command(43)),
        "another member is unaffected"
    );
    wait_for(
        || {
            mock.requests()
                .iter()
                .filter(|r| r.path.contains("/token-42/"))
                .count()
                == cap + 1
        },
        "capped member's work plus one busy reply",
    )
    .await;
    mock.shutdown().await;
}

#[tokio::test]
async fn reserved_lane_is_not_reachable_without_the_permission() {
    let (mock, origin) = MockRest::start_script(held_callbacks(16)).await;
    let runtime = runtime_without_db(gates(false, false), false, origin);
    let _guard = runtime.dispatch_guard();
    for member in 1..=16 {
        assert!(runtime.dispatch(&open_command(member)));
    }
    // Same command, same lane pressure, but the invoker lacks MODERATE_MEMBERS.
    let impostor = interaction_event(from_member(
        slash("timeout", Some(CHANNEL), Vec::new()),
        901,
        Permissions::SEND_MESSAGES,
    ));
    assert!(
        !runtime.dispatch(&impostor),
        "naming a moderation command is not enough"
    );
    wait_for(
        || {
            mock.requests()
                .iter()
                .any(|r| r.path.contains("/token-901/"))
        },
        "busy callback",
    )
    .await;
    let reply = callback_json(
        &mock
            .requests()
            .into_iter()
            .find(|r| r.path.contains("/token-901/"))
            .expect("callback"),
    );
    assert_eq!(
        reply["data"]["content"],
        crate::interaction_admission::BUSY_REPLY
    );
    mock.shutdown().await;
}

// ---------------------------------------------------------------------------
// Consolidated LFG + sticky/feed runtime (no database)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lfg_sticky_and_feed_routes_coexist_without_db() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime.set_identity(1111, 1111);

    // Each accepted route fails validation before SQL: LFG lacks required
    // options, sticky has no channel, and feeds lack required options (a
    // strict-decode refusal naming the option with a correction).
    // This exercises feature execution, not just registry coexistence.
    for (index, (name, channel, permissions, expected)) in [
        (
            "lfg",
            Some(CHANNEL),
            Permissions::MANAGE_EVENTS,
            "Missing title option.",
        ),
        (
            "lfg-close",
            Some(CHANNEL),
            Permissions::MANAGE_EVENTS,
            "Missing id option.",
        ),
        (
            "lfg-edit",
            Some(CHANNEL),
            Permissions::MANAGE_EVENTS,
            "Missing id option.",
        ),
        (
            "sticky",
            None,
            Permissions::MANAGE_GUILD,
            "This command only works in a channel.",
        ),
        (
            "sticky-remove",
            None,
            Permissions::MANAGE_GUILD,
            "This command only works in a channel.",
        ),
        (
            "feed-add",
            Some(CHANNEL),
            Permissions::MANAGE_GUILD,
            "Missing kind. Use /feed-add kind:rss source:<https-url>.",
        ),
        (
            "feed-remove",
            Some(CHANNEL),
            Permissions::MANAGE_GUILD,
            "Missing id. Use /feed-remove id:<relay-id>.",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut interaction = slash(name, channel, Vec::new());
        interaction.id = Id::new(7000 + index as u64);
        interaction.member.as_mut().unwrap().permissions = Some(permissions);
        tokio::time::timeout(Duration::from_secs(2), runtime.on_interaction(&interaction))
            .await
            .expect("validation must finish without waiting for the lazy pool");

        let requests = mock.requests();
        assert_eq!(
            requests.len(),
            2 * (index + 1),
            "only defer and edit for {name}"
        );
        let callback = &requests[2 * index];
        assert_eq!(callback.method, "POST");
        assert_eq!(
            callback.path,
            format!(
                "/api/v10/interactions/{}/sticky-test-token/callback",
                interaction.id
            )
        );
        let acknowledgement: serde_json::Value = serde_json::from_slice(&callback.body).unwrap();
        assert_eq!(
            acknowledgement["type"], 5,
            "one deferred callback for {name}"
        );
        assert_eq!(acknowledgement["data"]["flags"], 64);
        let edit = &requests[2 * index + 1];
        assert_eq!(edit.method, "PATCH");
        assert_eq!(
            edit.path,
            "/api/v10/webhooks/1111/sticky-test-token/messages/@original"
        );
        let reply: serde_json::Value = serde_json::from_slice(&edit.body).unwrap();
        assert_eq!(reply["content"], expected, "correct feature owns {name}");
        assert_eq!(reply["allowed_mentions"]["parse"], serde_json::json!([]));
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn lfg_list_store_failure_reaches_read_path_without_other_rest_effects() {
    for automations in [false, true] {
        let (mock, origin) = MockRest::start(Vec::new()).await;
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
            .expect("lazy pool");
        // Fail reads immediately without connecting to any database.
        pool.close().await;
        let runtime = CommandRuntime::new(
            pool,
            executor_at(origin),
            router_with_commands(gates(automations, true)),
            GUILD,
            automations,
        );
        let mut interaction = slash("lfg-list", Some(CHANNEL), Vec::new());
        interaction.member.as_mut().unwrap().permissions = Some(Permissions::MANAGE_EVENTS);
        tokio::time::timeout(Duration::from_secs(2), runtime.on_interaction(&interaction))
            .await
            .expect("unavailable list must finish with a bounded reply");

        let requests = mock.requests();
        assert_eq!(
            requests.len(),
            1,
            "only the callback; no identity, channel, history, defer/edit or mutation REST"
        );
        let callback = &requests[0];
        assert_eq!(callback.method, "POST");
        assert_eq!(
            callback.path,
            "/api/v10/interactions/7/sticky-test-token/callback"
        );
        let reply: serde_json::Value = serde_json::from_slice(&callback.body).unwrap();
        assert_eq!(reply["type"], 4, "one immediate callback, not a defer");
        assert_eq!(reply["data"]["flags"], 64, "ephemeral discovery reply");
        assert_eq!(
            reply["data"]["content"], "Unable to list game posts. Try /lfg-list again.",
            "the real read path owns the command even with automations={automations}"
        );
        assert_eq!(
            reply["data"]["allowed_mentions"]["parse"],
            serde_json::json!([])
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn lfg_commands_preserve_manage_events_and_disabled_refusals() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    for (announcements, permissions, expected) in [
        (true, Some(Permissions::empty()), MANAGE_EVENTS_REQUIRED),
        (
            true,
            Some(Permissions::MANAGE_GUILD),
            MANAGE_EVENTS_REQUIRED,
        ),
        (true, None, MANAGE_EVENTS_REQUIRED),
        (
            false,
            Some(Permissions::MANAGE_EVENTS),
            ANNOUNCEMENTS_DISABLED_REPLY,
        ),
    ] {
        let runtime = runtime_without_db(gates(true, announcements), true, origin.clone());
        runtime.set_identity(1111, 1111);
        for name in ["lfg", "lfg-close", "lfg-edit"] {
            let mut interaction = slash(name, Some(CHANNEL), Vec::new());
            interaction.member.as_mut().unwrap().permissions = permissions;
            let before = mock.requests().len();
            runtime.on_interaction(&interaction).await;
            let requests = mock.requests();
            assert_eq!(requests.len(), before + 1, "one refusal, no defer/effects");
            let callback = requests.last().unwrap();
            assert_eq!(callback.method, "POST");
            assert_eq!(
                callback.path,
                "/api/v10/interactions/7/sticky-test-token/callback"
            );
            let reply: serde_json::Value = serde_json::from_slice(&callback.body).unwrap();
            assert_eq!(reply["type"], 4);
            assert_eq!(reply["data"]["flags"], 64);
            assert_eq!(
                reply["data"]["content"], expected,
                "router refusal for {name}"
            );
        }
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn ordinary_member_lfg_select_reaches_the_feature_before_db() {
    for values in [Vec::new(), vec!["tank".to_owned(), "dps".to_owned()]] {
        let (mock, origin) = MockRest::start(Vec::new()).await;
        let runtime = runtime_without_db(gates(true, true), true, origin);
        runtime.set_identity(1111, 1111);
        // Zero admin permissions are intentional: signup is not gated by
        // ManageEvents/ManageGuild. Invalid cardinality avoids store access.
        let interaction = lfg_select(values);
        tokio::time::timeout(Duration::from_secs(2), runtime.on_interaction(&interaction))
            .await
            .expect("invalid select must not wait for the lazy pool");
        assert_eq!(mock.deferred_reply()["content"], "Choose one LFG role.");
        assert_eq!(
            mock.requests().len(),
            2,
            "one defer, one completion; no effects"
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn consolidated_runtime_preserves_guild_fences_for_lfg_and_siblings() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime.set_identity(1111, 1111);
    for guild in [Some(Id::new(9999)), None] {
        for name in [
            "lfg",
            "lfg-close",
            "lfg-edit",
            "sticky",
            "sticky-remove",
            "feed-add",
            "feed-remove",
            "feed-list",
        ] {
            let mut interaction = slash(name, Some(CHANNEL), Vec::new());
            set_guild(&mut interaction, guild);
            interaction.member.as_mut().unwrap().permissions =
                Some(Permissions::MANAGE_EVENTS | Permissions::MANAGE_GUILD);
            runtime.on_interaction(&interaction).await;
        }
        let mut interaction = lfg_select(Vec::new());
        set_guild(&mut interaction, guild);
        runtime.on_interaction(&interaction).await;
    }
    assert!(
        mock.requests().is_empty(),
        "foreign guild/DM routes stay silent"
    );

    // Moderation's guild fence remains an explicit refusal, unlike the
    // announcement/automation routes above, even with its feature gate off.
    let mut interaction = slash("ban", Some(CHANNEL), Vec::new());
    set_guild(&mut interaction, Some(Id::new(9999)));
    runtime.on_interaction(&interaction).await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "only the moderation guild refusal");
    assert_eq!(requests[0].method, "POST");
    assert!(requests[0].path.ends_with("/callback"));
    let reply: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(reply["type"], 4);
    assert_eq!(reply["data"]["flags"], 64);
    assert_eq!(
        reply["data"]["content"],
        RouterRefusal::GuildRestricted.message()
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn production_build_requires_authoritative_custom_rows_before_full_publication() {
    let expected_names: Vec<String> = router_with_commands(gates(true, true))
        .publish_set(&[])
        .expect("complete registry")
        .into_iter()
        .map(|command| command.name)
        .collect();
    for custom_commands in [None, Some(Vec::new())] {
        let has_authoritative_rows = custom_commands.is_some();
        let (mock, origin) = MockRest::start(Vec::new()).await;
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
            .expect("lazy pool");
        pool.close().await;
        // Use the production constructor, not `new`'s Some(empty) fixture.
        // Closing the lazy pool proves publication needs no DB connection.
        let executor = executor_at(origin);
        let leveling = two_bot_discord::LevelingRuntime::new(
            pool.clone(),
            Arc::new(executor.clone()),
            GUILD,
            two_bot_core::OnboardingGates {
                mode: two_bot_core::OnboardingMode::Legacy,
                dry_run: false,
            },
        );
        let runtime = CommandRuntime::build(
            pool,
            executor,
            router_with_commands(gates(true, true)),
            None,
            leveling,
            GUILD,
            custom_commands,
            None,
            None,
        );
        runtime.set_identity(1111, 1111);
        runtime.publish_registry(Some(1111)).await;
        let requests = mock.requests();
        if has_authoritative_rows {
            assert_eq!(requests.len(), 1, "one full replacement");
            assert_eq!(requests[0].method, "PUT");
            assert_eq!(
                requests[0].path,
                "/api/v10/applications/1111/guilds/2222/commands"
            );
            let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
            let names: Vec<String> = body
                .as_array()
                .expect("command array")
                .iter()
                .map(|command| command["name"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(
                names, expected_names,
                "publish every gated builtin, not LFG-only"
            );
            for name in [
                "lfg",
                "lfg-close",
                "lfg-edit",
                "sticky",
                "sticky-remove",
                "feed-add",
                "feed-remove",
                "feed-list",
            ] {
                assert!(
                    names.iter().any(|published| published == name),
                    "missing {name}"
                );
            }
        } else {
            assert!(
                requests.is_empty(),
                "unavailable custom store must not replace the registry"
            );
        }
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn boot_publication_failure_is_reported_and_remains_retryable() {
    let (mock, origin) = MockRest::start_script(vec![
        RestResponse::status(403),
        RestResponse {
            status: 200,
            body: Some(r#"{"id":"1111"}"#.to_owned()),
            delay: Duration::ZERO,
        },
    ])
    .await;
    let runtime = runtime_without_db(gates(false, true), false, origin);
    runtime.set_identity(1111, 1111);
    let error = runtime
        .publish_registry_checked(Some(1111))
        .await
        .unwrap_err();
    assert_eq!(error, RegistrySyncError::Publish);
    runtime.publish_registry_checked(None).await.unwrap();
    runtime.publish_registry_checked(None).await.unwrap();
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        3,
        "failed sync retries; successful RESUME sync runs once"
    );
    assert_eq!(requests[0].method, "PUT");
    assert_eq!(requests[1].method, "GET");
    assert_eq!(requests[2].method, "PUT");
    mock.shutdown().await;
}

#[tokio::test]
async fn foreign_application_interactions_never_reach_a_feature() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime.set_identity(1111, 1111);
    for name in ["lfg", "lfg-close", "lfg-edit", "sticky", "feed-add"] {
        let mut interaction = slash(name, Some(CHANNEL), Vec::new());
        interaction.application_id = Id::new(9999);
        interaction.member.as_mut().unwrap().permissions =
            Some(Permissions::MANAGE_EVENTS | Permissions::MANAGE_GUILD);
        runtime.on_interaction(&interaction).await;
    }
    let mut interaction = lfg_select(Vec::new());
    interaction.application_id = Id::new(9999);
    runtime.on_interaction(&interaction).await;
    assert!(
        mock.requests().is_empty(),
        "application fence precedes all execution"
    );
    mock.shutdown().await;
}

// ---------------------------------------------------------------------------
// Feed slice units (no database)
// ---------------------------------------------------------------------------

#[test]
fn feed_add_options_strict_decodes_valid_and_ignores_unknown_names() {
    let interaction = slash(
        "feed-add",
        Some(CHANNEL),
        vec![
            option("kind", CommandOptionValue::String("rss".to_owned())),
            option(
                "source",
                CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
            ),
            option("other", CommandOptionValue::Boolean(true)),
        ],
    );
    assert_eq!(
        feed_add_options_strict(&interaction),
        Ok(("rss".to_owned(), "https://example.com/feed.xml".to_owned()))
    );
}

#[test]
fn feed_add_options_strict_refuses_wrong_types_duplicates_and_missing_options() {
    let wrong_kind = slash(
        "feed-add",
        Some(CHANNEL),
        vec![
            option("kind", CommandOptionValue::Integer(1)),
            option(
                "source",
                CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
            ),
        ],
    );
    assert_eq!(
        feed_add_options_strict(&wrong_kind),
        Err("Invalid kind. Use /feed-add kind:rss source:<https-url>.")
    );
    let wrong_source = slash(
        "feed-add",
        Some(CHANNEL),
        vec![
            option("kind", CommandOptionValue::String("rss".to_owned())),
            option("source", CommandOptionValue::Integer(7)),
        ],
    );
    assert_eq!(
        feed_add_options_strict(&wrong_source),
        Err("Invalid source. Use /feed-add kind:rss source:<https-url>.")
    );
    let duplicate_kind = slash(
        "feed-add",
        Some(CHANNEL),
        vec![
            option("kind", CommandOptionValue::String("rss".to_owned())),
            option("kind", CommandOptionValue::String("rss".to_owned())),
            option(
                "source",
                CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
            ),
        ],
    );
    assert_eq!(
        feed_add_options_strict(&duplicate_kind),
        Err("Duplicate kind. Use /feed-add kind:rss source:<https-url>.")
    );
    // Duplicates win over wrong types, and `kind` wins over `source`.
    let duplicate_wins = slash(
        "feed-add",
        Some(CHANNEL),
        vec![
            option("kind", CommandOptionValue::Integer(1)),
            option(
                "source",
                CommandOptionValue::String("https://example.com/a.xml".to_owned()),
            ),
            option(
                "source",
                CommandOptionValue::String("https://example.com/b.xml".to_owned()),
            ),
        ],
    );
    assert_eq!(
        feed_add_options_strict(&duplicate_wins),
        Err("Duplicate source. Use /feed-add kind:rss source:<https-url>.")
    );
    // Wrong types beat missing options across options: a present-but-unusable
    // value is named even when the other option is absent.
    let wrong_type_beats_missing = slash(
        "feed-add",
        Some(CHANNEL),
        vec![option("source", CommandOptionValue::Integer(7))],
    );
    assert_eq!(
        feed_add_options_strict(&wrong_type_beats_missing),
        Err("Invalid source. Use /feed-add kind:rss source:<https-url>.")
    );
    let wrong_kind_alone = slash(
        "feed-add",
        Some(CHANNEL),
        vec![option("kind", CommandOptionValue::Integer(1))],
    );
    assert_eq!(
        feed_add_options_strict(&wrong_kind_alone),
        Err("Invalid kind. Use /feed-add kind:rss source:<https-url>.")
    );
    let missing_kind = slash(
        "feed-add",
        Some(CHANNEL),
        vec![option(
            "source",
            CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
        )],
    );
    assert_eq!(
        feed_add_options_strict(&missing_kind),
        Err("Missing kind. Use /feed-add kind:rss source:<https-url>.")
    );
    let missing_source = slash(
        "feed-add",
        Some(CHANNEL),
        vec![option("kind", CommandOptionValue::String("rss".to_owned()))],
    );
    assert_eq!(
        feed_add_options_strict(&missing_source),
        Err("Missing source. Use /feed-add kind:rss source:<https-url>.")
    );
    let mut no_data = slash("feed-add", Some(CHANNEL), Vec::new());
    no_data.data = None;
    assert_eq!(
        feed_add_options_strict(&no_data),
        Err("Missing kind. Use /feed-add kind:rss source:<https-url>.")
    );
}

#[test]
fn feed_id_option_extracts_id_for_inspect() {
    // `/feed-remove` decodes strictly via `feed_remove_option_strict`; the
    // lenient helper serves `/feed-inspect` only.
    let name = "feed-inspect";
    let interaction = slash(
        name,
        Some(CHANNEL),
        vec![option(
            "id",
            CommandOptionValue::String("feed-1".to_owned()),
        )],
    );
    assert_eq!(
        feed_id_option(&interaction),
        Some("feed-1".to_owned()),
        "{name} decodes its id"
    );
    let missing = slash(name, Some(CHANNEL), Vec::new());
    assert_eq!(feed_id_option(&missing), None, "{name} id may be absent");
}

#[test]
fn feed_remove_option_strict_decodes_valid_and_ignores_unknown_names() {
    let interaction = slash(
        "feed-remove",
        Some(CHANNEL),
        vec![
            option("id", CommandOptionValue::String("feed-1".to_owned())),
            option("other", CommandOptionValue::Boolean(true)),
        ],
    );
    assert_eq!(
        feed_remove_option_strict(&interaction),
        Ok("feed-1".to_owned())
    );
    // Present-but-empty still decodes: the planner's `validate_id` owns
    // that refusal.
    let empty = slash(
        "feed-remove",
        Some(CHANNEL),
        vec![option("id", CommandOptionValue::String(String::new()))],
    );
    assert_eq!(feed_remove_option_strict(&empty), Ok(String::new()));
}

#[test]
fn feed_remove_option_strict_refuses_wrong_type_duplicate_and_missing_id() {
    let wrong_type = slash(
        "feed-remove",
        Some(CHANNEL),
        vec![option("id", CommandOptionValue::Integer(42))],
    );
    assert_eq!(
        feed_remove_option_strict(&wrong_type),
        Err("Invalid id. Use /feed-remove id:<relay-id>.")
    );
    let duplicate = slash(
        "feed-remove",
        Some(CHANNEL),
        vec![
            option("id", CommandOptionValue::String("feed-1".to_owned())),
            option("id", CommandOptionValue::String("feed-1".to_owned())),
        ],
    );
    assert_eq!(
        feed_remove_option_strict(&duplicate),
        Err("Duplicate id. Use /feed-remove id:<relay-id>.")
    );
    let missing = slash("feed-remove", Some(CHANNEL), Vec::new());
    assert_eq!(
        feed_remove_option_strict(&missing),
        Err("Missing id. Use /feed-remove id:<relay-id>.")
    );
    let mut no_data = slash("feed-remove", Some(CHANNEL), Vec::new());
    no_data.data = None;
    assert_eq!(
        feed_remove_option_strict(&no_data),
        Err("Missing id. Use /feed-remove id:<relay-id>.")
    );
}

#[test]
fn feed_list_page_request_defaults_and_refuses_unusable_values() {
    let missing = slash("feed-list", Some(CHANNEL), Vec::new());
    assert_eq!(feed_list_page_request(&missing), FeedListPage::Default);
    for (value, expected) in [
        (CommandOptionValue::Integer(1), FeedListPage::Page(1)),
        (CommandOptionValue::Integer(2), FeedListPage::Page(2)),
        (
            CommandOptionValue::Integer(i64::MAX),
            FeedListPage::Page(i64::MAX),
        ),
        // Range is refused by the reply composer, not the decoder.
        (CommandOptionValue::Integer(0), FeedListPage::Page(0)),
        (CommandOptionValue::Integer(-2), FeedListPage::Page(-2)),
    ] {
        let interaction = slash("feed-list", Some(CHANNEL), vec![option("page", value)]);
        assert_eq!(feed_list_page_request(&interaction), expected);
    }
    // A present option with a non-integer wire value must refuse, never
    // silently list the default page.
    for value in [
        CommandOptionValue::String("second".to_owned()),
        CommandOptionValue::Number(1.5),
        CommandOptionValue::Boolean(true),
    ] {
        let interaction = slash("feed-list", Some(CHANNEL), vec![option("page", value)]);
        assert_eq!(feed_list_page_request(&interaction), FeedListPage::Invalid);
    }
}

#[test]
fn help_command_selector_keeps_omitted_absent_and_malformed_present() {
    // An omitted option means the grouped index.
    let missing = slash("help", Some(CHANNEL), Vec::new());
    assert_eq!(help_command_selector(&missing), None);
    // A string selector passes through untouched.
    let named = slash(
        "help",
        Some(CHANNEL),
        vec![option(
            "command",
            CommandOptionValue::String("rank".to_owned()),
        )],
    );
    assert_eq!(help_command_selector(&named), Some("rank".to_owned()));
    // A present option with a non-string wire value is malformed: decode to
    // an empty selector (the unknown-name correction), never the index.
    for value in [
        CommandOptionValue::Integer(1),
        CommandOptionValue::Number(1.5),
        CommandOptionValue::Boolean(true),
    ] {
        let interaction = slash("help", Some(CHANNEL), vec![option("command", value)]);
        assert_eq!(help_command_selector(&interaction), Some(String::new()));
    }
}

#[test]
fn new_id_is_unique_hex_that_validate_id_accepts() {
    let first = new_id();
    let second = new_id();
    assert_eq!(first.len(), 32);
    assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_ne!(first, second);
}

// ---------------------------------------------------------------------------
// Schedule slice units (no database)
// ---------------------------------------------------------------------------

#[test]
fn schedule_options_extract_body_and_timings() {
    let interaction = slash(
        "schedule",
        Some(CHANNEL),
        vec![
            option("body", CommandOptionValue::String("hello".to_owned())),
            option("in-minutes", CommandOptionValue::Integer(30)),
            option("every-minutes", CommandOptionValue::Integer(90)),
        ],
    );
    assert_eq!(
        schedule_options(&interaction),
        (Some("hello".to_owned()), Some(30), Some(90))
    );
}

#[test]
fn schedule_options_missing_values_decode_to_none() {
    let interaction = slash("schedule", Some(CHANNEL), Vec::new());
    assert_eq!(schedule_options(&interaction), (None, None, None));
}

#[test]
fn schedule_remove_option_extracts_id() {
    let interaction = slash(
        "schedule-remove",
        Some(CHANNEL),
        vec![option(
            "id",
            CommandOptionValue::String("abc123".to_owned()),
        )],
    );
    assert_eq!(
        schedule_remove_option(&interaction),
        Some("abc123".to_owned())
    );
    let missing = slash("schedule-remove", Some(CHANNEL), Vec::new());
    assert_eq!(schedule_remove_option(&missing), None);
}

// Exact value pins for the production strict decoders: the lenient helpers
// above are test-only, so these are what catch a corrupted body, a scaled
// delay or a swapped timing field on the path the handlers actually run.
#[test]
fn schedule_options_strict_decodes_exact_distinct_values() {
    let interaction = slash(
        "schedule",
        Some(CHANNEL),
        vec![
            option("body", CommandOptionValue::String("hello".to_owned())),
            option("in-minutes", CommandOptionValue::Integer(30)),
            option("every-minutes", CommandOptionValue::Integer(90)),
        ],
    );
    assert_eq!(
        schedule_options_strict(&interaction),
        Ok((Some("hello".to_owned()), Some(30), Some(90)))
    );
    // Option order must not change which value lands in which slot.
    let reordered = slash(
        "schedule",
        Some(CHANNEL),
        vec![
            option("every-minutes", CommandOptionValue::Integer(90)),
            option("in-minutes", CommandOptionValue::Integer(30)),
            option("body", CommandOptionValue::String("hello".to_owned())),
        ],
    );
    assert_eq!(
        schedule_options_strict(&reordered),
        Ok((Some("hello".to_owned()), Some(30), Some(90)))
    );
}

#[test]
fn schedule_options_strict_decodes_single_timing_and_missing() {
    let only_in = slash(
        "schedule",
        Some(CHANNEL),
        vec![
            option("body", CommandOptionValue::String("ping".to_owned())),
            option("in-minutes", CommandOptionValue::Integer(30)),
        ],
    );
    assert_eq!(
        schedule_options_strict(&only_in),
        Ok((Some("ping".to_owned()), Some(30), None))
    );
    let only_every = slash(
        "schedule",
        Some(CHANNEL),
        vec![
            option("body", CommandOptionValue::String("ping".to_owned())),
            option("every-minutes", CommandOptionValue::Integer(45)),
        ],
    );
    assert_eq!(
        schedule_options_strict(&only_every),
        Ok((Some("ping".to_owned()), None, Some(45)))
    );
    let none = slash("schedule", Some(CHANNEL), Vec::new());
    assert_eq!(schedule_options_strict(&none), Ok((None, None, None)));
}

#[test]
fn schedule_remove_and_page_strict_decode_exact_values() {
    let remove = slash(
        "schedule-remove",
        Some(CHANNEL),
        vec![option(
            "id",
            CommandOptionValue::String("abc123".to_owned()),
        )],
    );
    assert_eq!(
        schedule_remove_option_strict(&remove),
        Ok(Some("abc123".to_owned()))
    );
    let missing = slash("schedule-remove", Some(CHANNEL), Vec::new());
    assert_eq!(schedule_remove_option_strict(&missing), Ok(None));
    let page = slash(
        "schedule-list",
        Some(CHANNEL),
        vec![option("page", CommandOptionValue::Integer(7))],
    );
    assert_eq!(schedule_list_page_option_strict(&page), Ok(Some(7)));
    let no_page = slash("schedule-list", Some(CHANNEL), Vec::new());
    assert_eq!(schedule_list_page_option_strict(&no_page), Ok(None));
}

#[tokio::test]
async fn feed_commands_refuse_ephemerally_while_announcements_off() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    // announcements OFF in the router gates → the shared router refuses all
    // six feed commands; the runtime answers through the shared executor.
    let runtime = runtime_without_db(gates(true, false), true, origin);
    for name in [
        "feed-add",
        "feed-remove",
        "feed-pause",
        "feed-resume",
        "feed-list",
        "feed-inspect",
    ] {
        runtime
            .on_interaction(&slash(name, Some(CHANNEL), Vec::new()))
            .await;
    }
    let requests = mock.requests();
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 6, "one ephemeral refusal per command");
    assert_eq!(
        requests.len(),
        6,
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
    for name in [
        "feed-add",
        "feed-remove",
        "feed-pause",
        "feed-resume",
        "feed-list",
        "feed-inspect",
    ] {
        let mut interaction = slash(name, Some(CHANNEL), Vec::new());
        set_guild(&mut interaction, Some(Id::new(9999)));
        runtime.on_interaction(&interaction).await;
    }
    assert!(
        mock.requests().is_empty(),
        "wrong-guild interactions get silence, not a refusal"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn schedule_commands_refuse_ephemerally_while_automations_off() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    // automations OFF in the router gates → the shared router refuses all
    // three schedule commands; the runtime answers through the shared executor.
    let runtime = runtime_without_db(gates(false, true), false, origin);
    for name in ["schedule", "schedule-remove", "schedule-list"] {
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
        assert_eq!(json["data"]["content"], AUTOMATIONS_DISABLED_REPLY);
        assert_eq!(json["data"]["flags"], 64, "ephemeral");
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn schedule_without_manage_server_is_refused() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    let mut interaction = slash(
        "schedule",
        Some(CHANNEL),
        vec![
            option("body", CommandOptionValue::String("hello".to_owned())),
            option("in-minutes", CommandOptionValue::Integer(30)),
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
async fn schedule_commands_from_other_guilds_are_silent() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    for name in ["schedule", "schedule-remove", "schedule-list"] {
        let mut interaction = slash(name, Some(CHANNEL), Vec::new());
        set_guild(&mut interaction, Some(Id::new(9999)));
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
        "Feed source must be an HTTPS URL on port 443, without credentials, and at most 2048 bytes."
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn feed_remove_without_id_names_the_missing_option() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash("feed-remove", Some(CHANNEL), Vec::new()))
        .await;
    // A missing `id` is a strict-decode refusal with a correction, not the
    // planner's `InvalidId`: present-but-empty still replies "Invalid feed
    // id." via `validate_id`.
    assert_eq!(
        mock.deferred_reply()["content"],
        "Missing id. Use /feed-remove id:<relay-id>."
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn feed_add_without_kind_names_the_missing_option() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash(
            "feed-add",
            Some(CHANNEL),
            vec![option(
                "source",
                CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
            )],
        ))
        .await;
    assert_eq!(
        mock.deferred_reply()["content"],
        "Missing kind. Use /feed-add kind:rss source:<https-url>."
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn sticky_without_body_names_the_missing_option() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash(
            "sticky",
            Some(CHANNEL),
            vec![option("debounce", CommandOptionValue::Integer(7))],
        ))
        .await;
    // A missing `body` refuses before the existence read and the `rejected`
    // audit; present-but-empty still validates (and audits) as before.
    assert_eq!(
        mock.deferred_reply()["content"],
        "Missing body. Use /sticky body:<text> debounce:<seconds>."
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn feed_add_wrong_typed_source_names_the_option_at_dispatch() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash(
            "feed-add",
            Some(CHANNEL),
            vec![
                option("kind", CommandOptionValue::String("rss".to_owned())),
                option("source", CommandOptionValue::Integer(7)),
            ],
        ))
        .await;
    // Exact dispatch pin: a fixed-text regression (e.g. always replying the
    // missing-kind text) must fail here even though the corrected invocation
    // names both options.
    assert_eq!(
        mock.deferred_reply()["content"],
        "Invalid source. Use /feed-add kind:rss source:<https-url>."
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn feed_add_duplicated_kind_names_the_option_at_dispatch() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash(
            "feed-add",
            Some(CHANNEL),
            vec![
                option("kind", CommandOptionValue::String("rss".to_owned())),
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
        "Duplicate kind. Use /feed-add kind:rss source:<https-url>."
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn feed_remove_wrong_typed_id_names_the_option_at_dispatch() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash(
            "feed-remove",
            Some(CHANNEL),
            vec![option("id", CommandOptionValue::Integer(7))],
        ))
        .await;
    assert_eq!(
        mock.deferred_reply()["content"],
        "Invalid id. Use /feed-remove id:<relay-id>."
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn feed_remove_duplicated_id_names_the_option_at_dispatch() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash(
            "feed-remove",
            Some(CHANNEL),
            vec![
                option("id", CommandOptionValue::String("feed-1".to_owned())),
                option("id", CommandOptionValue::String("feed-1".to_owned())),
            ],
        ))
        .await;
    assert_eq!(
        mock.deferred_reply()["content"],
        "Duplicate id. Use /feed-remove id:<relay-id>."
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn sticky_wrong_typed_body_names_the_option_at_dispatch() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash(
            "sticky",
            Some(CHANNEL),
            vec![option("body", CommandOptionValue::Integer(7))],
        ))
        .await;
    assert_eq!(
        mock.deferred_reply()["content"],
        "Invalid body. Use /sticky body:<text> debounce:<seconds>."
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn sticky_duplicated_debounce_names_the_option_at_dispatch() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime
        .on_interaction(&slash(
            "sticky",
            Some(CHANNEL),
            vec![
                option("body", CommandOptionValue::String("remember".to_owned())),
                option("debounce", CommandOptionValue::Integer(7)),
                option("debounce", CommandOptionValue::Integer(7)),
            ],
        ))
        .await;
    assert_eq!(
        mock.deferred_reply()["content"],
        "Duplicate debounce. Use /sticky body:<text> debounce:<seconds>."
    );
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
        "giveaway",
        "rsvp",
        "rsvp-attendance",
        "lfg",
        "lfg-close",
        "lfg-edit",
        "lfg-list",
        "feed-add",
        "feed-remove",
        "feed-list",
        "feed-inspect",
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
    for withheld in [
        "feed-add",
        "feed-remove",
        "feed-pause",
        "feed-resume",
        "feed-list",
        "feed-inspect",
        "rsvp",
        "lfg",
        "lfg-close",
        "lfg-edit",
        "lfg-list",
    ] {
        assert!(
            !names.contains(&withheld),
            "gated-off {withheld} must not publish"
        );
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn ready_publish_carries_the_voice_set_only_while_voice_is_live() {
    for (voice, assistant) in [(false, true), (true, false), (true, true)] {
        let (mock, origin) = MockRest::start(Vec::new()).await;
        let runtime = runtime_without_db(
            RouterGates {
                voice,
                voice_assistant: assistant,
                ..gates(true, true)
            },
            true,
            origin,
        );
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
        for name in VOICE_ONLY_NAMES {
            assert_eq!(
                names.contains(&name),
                voice,
                "/{name} publishes iff voice is live (voice={voice})"
            );
        }
        assert!(
            !names.contains(&"templateassistant"),
            "/templateassistant stays unpublished until its slice lands (voice={voice} assistant={assistant})"
        );
        if voice {
            let import = body
                .as_array()
                .unwrap()
                .iter()
                .find(|command| command["name"] == "import")
                .unwrap();
            assert_eq!(
                import["options"][0]["type"], 11,
                "upload stays an attachment"
            );
        }
        // The non-voice registry is untouched either way.
        for expected in [
            "rank",
            "leaderboard",
            "game-mix",
            "sticky",
            "rsvp",
            "feed-list",
        ] {
            assert!(names.contains(&expected), "{expected} must still publish");
        }
        mock.shutdown().await;
    }
}

#[test]
fn production_router_matches_composed_production_wiring() {
    use two_bot_core::{HandlerId, InteractionHandler, InteractionRouter, ModerationAction};

    #[derive(Debug)]
    struct Marker(HandlerId);
    impl InteractionHandler for Marker {
        fn id(&self) -> HandlerId {
            self.0
        }
    }

    fn every_handler_id() -> Vec<HandlerId> {
        let mut ids = vec![
            HandlerId::Rank,
            HandlerId::Leaderboard,
            HandlerId::Help,
            HandlerId::Privacy,
            HandlerId::Reopen,
            HandlerId::WelcomePreview,
            HandlerId::GameMix,
            HandlerId::ScorecardAttendance,
            HandlerId::AutomationAdmin,
            HandlerId::AutomationCustom,
            HandlerId::Giveaway,
            HandlerId::Rsvp,
            HandlerId::RsvpAttendance,
            HandlerId::Lfg,
            HandlerId::LfgClose,
            HandlerId::LfgEdit,
            HandlerId::LfgList,
            HandlerId::FeedAdd,
            HandlerId::FeedRemove,
            HandlerId::FeedPause,
            HandlerId::FeedResume,
            HandlerId::FeedList,
            HandlerId::FeedInspect,
            HandlerId::PurgePreview,
            HandlerId::ChannelInspect,
        ];
        ids.extend(ModerationAction::ALL.map(HandlerId::Moderation));
        ids
    }

    // Composition mirrors production: the real `router_with_commands`, the
    // member verbs `CommandRuntime::build` adds while the member consumer
    // exists, and the LFG handlers `InteractionRuntime::with_router` adds. Only
    // `handler_for` presence is compared.
    fn composed(gates: RouterGates, member_present: bool) -> InteractionRouter {
        let mut router = router_with_commands(gates);
        if member_present {
            for action in [
                ModerationAction::Ban,
                ModerationAction::TempBan,
                ModerationAction::Kick,
                ModerationAction::Timeout,
                ModerationAction::Warn,
            ] {
                router.register(Box::new(Marker(HandlerId::Moderation(action))));
            }
        }
        router.register(Box::new(Marker(HandlerId::Lfg)));
        router.register(Box::new(Marker(HandlerId::LfgClose)));
        router.register(Box::new(Marker(HandlerId::LfgEdit)));
        router.register(Box::new(Marker(HandlerId::LfgList)));
        router
    }

    let all_on = RouterGates {
        configured_guild: Some(GUILD),
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
    };
    // Every registration gate on/off, crossed with the member consumer.
    for scorecard in [false, true] {
        for automations in [false, true] {
            for announcements in [false, true] {
                for moderation in [false, true] {
                    for member_present in [false, true] {
                        let gates = RouterGates {
                            scorecard,
                            automations,
                            announcements,
                            moderation,
                            ..all_on
                        };
                        let real = composed(gates, member_present);
                        let seam = InteractionRouter::production_router(gates, member_present);
                        // Equal absence is not conformance: both constructors
                        // must register the dedicated handler while enabled.
                        for (name, router) in [("composed", &real), ("seam", &seam)] {
                            assert_eq!(
                                router.handler_for(&HandlerId::Giveaway).map(|h| h.id()),
                                automations.then_some(HandlerId::Giveaway),
                                "{name} giveaway registration follows automations \
                                 (scorecard={scorecard} automations={automations} \
                                 announcements={announcements} moderation={moderation} \
                                 member={member_present})"
                            );
                            assert_eq!(
                                router.handler_for(&HandlerId::FeedInspect).map(|h| h.id()),
                                announcements.then_some(HandlerId::FeedInspect),
                                "{name} feed-inspect registration follows announcements \
                                 (scorecard={scorecard} automations={automations} \
                                 announcements={announcements} moderation={moderation} \
                                 member={member_present})"
                            );
                        }
                        for id in every_handler_id() {
                            assert_eq!(
                                real.handler_for(&id).is_some(),
                                seam.handler_for(&id).is_some(),
                                "{id:?} registration drifted (scorecard={scorecard} \
                                 automations={automations} announcements={announcements} \
                                 moderation={moderation} member={member_present})"
                            );
                        }
                        // The exception list is the same five verbs the owner
                        // map guards on the consumer, and nothing else.
                        let exceptions =
                            InteractionRouter::intentional_registration_exceptions(member_present);
                        assert_eq!(
                            exceptions.len(),
                            if member_present { 0 } else { 5 },
                            "member={member_present}"
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn duplicate_ready_republishes_the_identical_set() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    runtime.publish_registry(Some(1111)).await;
    runtime.publish_registry(Some(1111)).await;
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
        for name in [
            "feed-add",
            "feed-remove",
            "feed-pause",
            "feed-resume",
            "feed-list",
            "feed-inspect",
            "rsvp",
            "lfg",
            "lfg-list",
        ] {
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

#[tokio::test]
#[ignore = "requires disposable agent-testdb and mock REST only"]
async fn shared_runtime_routes_leveling_once_with_legacy_visibility_and_guild_fence() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test bootstrap URL");
    let db =
        two_bot_testsupport::TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("migrated test database; no credential fallback");
    let (mock, origin) = MockRest::start(vec![204, 200, 204, 200, 204, 200, 204, 200]).await;
    let executor = ActionExecutor::with_proxy("mock-only-token".into(), Some(origin)).unwrap();
    let runtime = CommandRuntime::new(
        db.pool().clone(),
        executor,
        router_with_commands(gates(false, false)),
        GUILD,
        false,
    );
    let pipeline = two_bot_discord::OrderedLevelingPipeline::new(
        two_bot_core::MemStore::new(),
        Some(runtime.leveling()),
    );
    let rank = slash("rank", Some(CHANNEL), Vec::new());
    let mut value = serde_json::to_value(&rank).unwrap();
    value["data"]["options"] = serde_json::json!([{"name":"member","type":6,"value":"88"}]);
    let mut target = user(88, false);
    target.global_name = Some("Target member".into());
    value["data"]["resolved"] = serde_json::json!({"users":{"88":target}});
    let optional_rank: Interaction = serde_json::from_value(value).unwrap();
    let leaderboard = slash("leaderboard", Some(CHANNEL), Vec::new());
    for interaction in [&rank, &optional_rank, &leaderboard] {
        let event = Event::InteractionCreate(Box::new(InteractionCreate(interaction.clone())));
        assert!(pipeline.handle(&event).await.unwrap().is_empty());
        runtime.on_interaction(interaction).await;
    }
    // The routed board reads the current season only: lifetime stays empty,
    // so every row below must come from `member_season_xp`.
    let season = two_bot_core::leveling::season_id_for_ms(now_millis_for_test());
    for member in 77..89 {
        sqlx::query("INSERT INTO member_season_xp (guild_id,season_id,member_id,seasonal_xp,updated_at) VALUES ($1,$2,$3,$4,NOW())")
            .bind(GUILD_S).bind(&season).bind(member.to_string()).bind(i64::from(member))
            .execute(db.pool()).await.unwrap();
    }
    runtime.on_interaction(&leaderboard).await;
    for name in ["rank", "leaderboard"] {
        let mut foreign = slash(name, Some(CHANNEL), Vec::new());
        set_guild(&mut foreign, Some(Id::new(9999)));
        runtime.on_interaction(&foreign).await;
    }
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        8,
        "exactly one defer plus one completion edit per accepted command"
    );
    let callbacks: Vec<&RestRequest> = requests
        .iter()
        .filter(|r| r.method == "POST" && r.path.ends_with("/callback"))
        .collect();
    let edits: Vec<&RestRequest> = requests
        .iter()
        .filter(|r| r.method == "PATCH" && r.path.ends_with("/messages/@original"))
        .collect();
    assert_eq!(callbacks.len(), 4, "one acknowledgement per command");
    assert_eq!(edits.len(), 4, "one completion edit per command");
    let defers: Vec<serde_json::Value> = callbacks
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    for defer in &defers {
        assert_eq!(defer["type"], 5, "leveling acknowledges with a defer");
    }
    for defer in &defers[..2] {
        assert_eq!(defer["data"]["flags"], 64, "rank defers ephemerally");
    }
    for defer in &defers[2..] {
        assert_ne!(defer["data"]["flags"], 64, "leaderboard defers publicly");
    }
    let replies: Vec<serde_json::Value> = edits
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    for (reply, name) in replies[..2].iter().zip(["member", "Target member"]) {
        assert_eq!(
            reply["content"],
            two_bot_core::leveling::rank_text(name, 0, None, 0, 0)
        );
    }
    assert_eq!(
        replies[2]["content"],
        two_bot_core::leveling::leaderboard_reply_for_season(&[], &season).content
    );
    for reply in &replies {
        assert_eq!(
            reply["allowed_mentions"]["parse"],
            serde_json::json!([]),
            "every completion edit suppresses mentions"
        );
    }
    assert_eq!(
        replies[3]["content"].as_str().unwrap().lines().count(),
        11,
        "no invented paging on the completed board"
    );
    assert!(!replies[3]["content"].as_str().unwrap().contains("<@77>"));
    drop(pipeline);
    drop(runtime);
    mock.shutdown().await;
    db.close().await.expect("drop own disposable database");
}

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
        let ci = std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
        let host = match url.as_str() {
            "postgres://agent_test:@agent-testdb:5432/agent_test" => "agent-testdb",
            "postgresql://agent_test@localhost:5432/agent_test" if ci => "localhost",
            _ => panic!("non-test database refused"),
        };
        assert!(std::env::var_os("PGOPTIONS").is_none());
        // Do not read .pgpass or inherit credentials, TLS or URL overrides.
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

    /// Seed a scheduled message directly (other-guild isolation rows,
    /// extra-id targets for ambiguous prefixes) without going through the
    /// command path under test.
    async fn seed_schedule(&self, guild_id: &str, id: &str) {
        // next_run_at/created_at/updated_at are NOT NULL TEXT without
        // defaults (0140_scheduled_messages.sql).
        sqlx::query(
            "INSERT INTO scheduled_messages
               (id, guild_id, channel_id, body, next_run_at, interval_seconds, enabled,
                created_by, created_at, updated_by, updated_at)
             VALUES ($1, $2, $3, 'remember this', '2030-01-01T00:00:00.000Z', NULL, TRUE,
                     '77', '2026-09-30T12:00:00.000Z', '77', '2026-09-30T12:00:00.000Z')",
        )
        .bind(id)
        .bind(guild_id)
        .bind(CHANNEL_S)
        .execute(&self.pool)
        .await
        .expect("seed scheduled message");
    }

    /// Seed a scheduled message with an explicit run time, interval and
    /// enabled flag (listing-render fixtures). Direct insert, so a fixture
    /// can hold more rows than the quota-admission write path allows.
    async fn seed_schedule_row(
        &self,
        guild_id: &str,
        id: &str,
        next_run_at: &str,
        interval_seconds: Option<i64>,
        enabled: bool,
    ) {
        sqlx::query(
            "INSERT INTO scheduled_messages
               (id, guild_id, channel_id, body, next_run_at, interval_seconds, enabled,
                created_by, created_at, updated_by, updated_at)
             VALUES ($1, $2, $3, 'remember this', $4, $5, $6,
                     '77', '2026-09-30T12:00:00.000Z', '77', '2026-09-30T12:00:00.000Z')",
        )
        .bind(id)
        .bind(guild_id)
        .bind(CHANNEL_S)
        .bind(next_run_at)
        .bind(interval_seconds)
        .bind(enabled)
        .execute(&self.pool)
        .await
        .expect("seed scheduled message row");
    }

    /// Seed one scheduled message with caller-chosen channel, timing,
    /// recurrence and enabled state (full-quota list fixtures need rows
    /// whose rendered lines carry realistic lengths and state markers).
    /// Renamed from `seed_schedule_row` when the bounded-listing acceptance
    /// tests landed their own helper under that name; call sites below use
    /// this fixture-row form.
    async fn seed_schedule_fixture_row(&self, guild_id: &str, row: &ScheduleListFixtureRow) {
        sqlx::query(
            "INSERT INTO scheduled_messages
               (id, guild_id, channel_id, body, next_run_at, interval_seconds, enabled,
                created_by, created_at, updated_by, updated_at)
             VALUES ($1, $2, $3, 'remember this', $4, $5, $6,
                     '77', '2026-09-30T12:00:00.000Z', '77', '2026-09-30T12:00:00.000Z')",
        )
        .bind(&row.id)
        .bind(guild_id)
        .bind(&row.channel_id)
        .bind(&row.next_run_at)
        .bind(row.interval_seconds)
        .bind(row.enabled)
        .execute(&self.pool)
        .await
        .expect("seed scheduled message row");
    }

    /// (id, channel_id) for this guild's scheduled messages, in list order.
    async fn schedule_rows(&self) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT id, channel_id FROM scheduled_messages
              WHERE guild_id = $1 ORDER BY next_run_at, id",
        )
        .bind(GUILD_S)
        .fetch_all(&self.pool)
        .await
        .expect("scheduled message rows")
    }

    /// (action, target_key, outcome) from the shared automation audit log.
    async fn schedule_audits(&self) -> Vec<(String, String, String)> {
        sqlx::query_as(
            "SELECT action, target_key, outcome FROM automation_audit_log
              WHERE guild_id = $1 ORDER BY created_at, id",
        )
        .bind(GUILD_S)
        .fetch_all(&self.pool)
        .await
        .expect("automation audit rows")
    }
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn channel_sticky_and_feed_commands_share_one_runtime_and_complete_registry() {
    let db = TestDb::new().await;
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        db.pool.clone(),
        executor_at(origin),
        router_with_commands(RouterGates {
            moderation: true,
            ..gates(true, true)
        }),
        GUILD,
        true,
    );
    runtime.publish_registry(Some(1111)).await;
    let published: serde_json::Value = serde_json::from_slice(&mock.requests()[0].body).unwrap();
    let names: Vec<_> = published
        .as_array()
        .unwrap()
        .iter()
        .map(|command| command["name"].as_str().unwrap())
        .collect();
    for name in [
        "slowmode",
        "purge",
        "lockdown",
        "unlock",
        "sticky",
        "feed-list",
        "rank",
    ] {
        assert!(names.contains(&name), "complete registry includes {name}");
    }
    for (id, name) in [
        (101, "slowmode"),
        (102, "sticky-remove"),
        (103, "feed-list"),
    ] {
        let options = if name == "slowmode" {
            vec![
                option("seconds", CommandOptionValue::Integer(0)),
                option(
                    "reason",
                    CommandOptionValue::String("shared runtime test".to_owned()),
                ),
            ]
        } else {
            Vec::new()
        };
        let mut interaction = slash(name, Some(CHANNEL), options);
        interaction.id = Id::new(id);
        interaction.member.as_mut().unwrap().permissions = Some(Permissions::all());
        runtime.on_interaction(&interaction).await;
    }
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 3, "each slice owns exactly one defer");
    for callback in callbacks {
        let body: serde_json::Value = serde_json::from_slice(&callback.body).unwrap();
        assert_eq!(body["type"], 5);
        assert_eq!(body["data"]["flags"], 64);
    }
    let requests = mock.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method == "PATCH" && r.path.ends_with("/@original"))
            .count(),
        3
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method == "PATCH" && r.path.ends_with("/channels/3333"))
            .count(),
        1
    );
    let audits: Vec<(String, String)> =
        sqlx::query_as("SELECT action, outcome FROM moderation_audit")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].0, "moderation.slowmode");
    assert_eq!(db.audits().await.len(), 1, "sticky still audited");
    assert!(
        db.feed_audits().await.is_empty(),
        "feed-list stays read-only"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn gateway_keeps_checkpointing_and_heartbeating_during_channel_rest_work() {
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio::sync::{mpsc, oneshot, RwLock};
    use tokio_websockets::{Message as WsMessage, ServerBuilder};
    use two_bot_cutover::gateway_session::GatewaySessionStore;

    let db = TestDb::new().await;
    let (mock, origin) = MockRest::start_script(vec![
        RestResponse::status(200), // READY registry publication.
        RestResponse::status(200), // Ephemeral defer.
        RestResponse {
            delay: Duration::from_secs(30), // Channel mutation, cancelled in flight.
            ..RestResponse::status(200)
        },
    ])
    .await;
    let runtime = CommandRuntime::new(
        db.pool.clone(),
        executor_at(origin),
        router_with_commands(RouterGates {
            moderation: true,
            ..gates(true, true)
        }),
        GUILD,
        true,
    );
    let mut interaction = slash(
        "slowmode",
        Some(CHANNEL),
        vec![
            option("seconds", CommandOptionValue::Integer(0)),
            option(
                "reason",
                CommandOptionValue::String("gateway test".to_owned()),
            ),
        ],
    );
    interaction.member.as_mut().unwrap().permissions = Some(Permissions::all());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let gateway_url = url.clone();
    let (published_tx, published_rx) = oneshot::channel();
    let (mutating_tx, mutating_rx) = oneshot::channel();
    let (heartbeat_tx, mut heartbeat_rx) = mpsc::channel(4);
    let gateway = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (_, mut ws) = ServerBuilder::new().accept(stream).await.unwrap();
        ws.send(WsMessage::text(
            serde_json::json!({"op":10,"d":{"heartbeat_interval":1000}}).to_string(),
        ))
        .await
        .unwrap();
        let mut published_rx = Some(published_rx);
        let mut mutating_rx = Some(mutating_rx);
        let mut dispatched = false;
        while let Some(Ok(message)) = ws.next().await {
            let Some(text) = message.as_text() else {
                continue;
            };
            let packet: serde_json::Value = serde_json::from_str(text).unwrap();
            match packet["op"].as_u64() {
                Some(2) => {
                    ws.send(WsMessage::text(
                        serde_json::json!({
                            "op":0,"s":1,"t":"READY","d":{
                                "v":10,"session_id":"channel-test","resume_gateway_url":gateway_url,
                                "guilds":[],"shard":[0,1],
                                "application":{"id":"1111","flags":0},
                                "user":{"id":"999","username":"mock","discriminator":"0",
                                    "avatar":null,"bot":true,"mfa_enabled":false,"verified":true}
                            }
                        })
                        .to_string(),
                    ))
                    .await
                    .unwrap();
                    published_rx.take().unwrap().await.unwrap();
                    ws.send(WsMessage::text(
                        serde_json::json!({
                            "op":0,"s":2,"t":"INTERACTION_CREATE","d":interaction
                        })
                        .to_string(),
                    ))
                    .await
                    .unwrap();
                    mutating_rx.take().unwrap().await.unwrap();
                    ws.send(WsMessage::text(serde_json::json!({
                        "op":0,"s":3,"t":"GUILD_MEMBER_REMOVE","d":{
                            "guild_id":GUILD_S,"user":{"id":"77","username":"mock-member","discriminator":"0"}
                        }
                    }).to_string())).await.unwrap();
                    dispatched = true;
                }
                Some(1) => {
                    ws.send(WsMessage::text("{\"op\":11,\"d\":null}".to_owned()))
                        .await
                        .unwrap();
                    if dispatched {
                        heartbeat_tx.send(()).await.unwrap();
                    }
                }
                _ => {}
            }
        }
    });
    crate::gateway::ensure_crypto_provider();
    let shard = crate::gateway::build_shard(
        "mock-token".to_owned(),
        twilight_gateway::Intents::empty(),
        None,
        Some(&url),
    );
    let store = GatewaySessionStore::new(db.pool.clone(), GUILD_S.to_owned(), 0);
    let runner = tokio::spawn(crate::gateway::run_shard(
        shard,
        Arc::new(crate::gateway::build_pipeline(Vec::new(), None)),
        Arc::new(RwLock::new(crate::gateway::GatewayState::Armed)),
        store.clone(),
        None,
        None,
        Some(Arc::clone(&runtime)),
        None,
        None,
        std::future::pending::<()>(),
    ));
    wait_for(|| mock.requests().len() == 1, "READY publication").await;
    published_tx.send(()).unwrap();
    wait_for(
        || {
            mock.requests()
                .iter()
                .any(|r| r.method == "PATCH" && r.path.ends_with("/channels/3333"))
        },
        "channel mutation entered",
    )
    .await;
    mutating_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if store.load().await.unwrap().is_some_and(|s| s.sequence == 3) {
                break;
            }
            tokio::task::yield_now().await;
        }
        heartbeat_rx.recv().await.unwrap();
    })
    .await
    .expect("gateway progressed while channel REST was pending");
    assert_eq!(
        mock.requests().len(),
        3,
        "effect has not completed or repeated"
    );
    runner.abort();
    assert!(runner.await.unwrap_err().is_cancelled());
    wait_for(
        || Arc::strong_count(&runtime) == 1,
        "command scope cancellation",
    )
    .await;
    let lanes: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_channel_executions")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(lanes, 1, "cancellation retains uncertain channel ownership");
    let state: String = sqlx::query_scalar("SELECT state FROM moderation_idempotency")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(state, "in_flight");
    assert!(
        !runtime.dispatch(&Event::InteractionCreate(Box::new(InteractionCreate(
            slash("slowmode", Some(CHANNEL), Vec::new()),
        ))))
    );
    gateway.abort();
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn disabled_channel_commands_defer_audit_and_refuse_without_channel_effects() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;
    let request = slash(
        "slowmode",
        Some(CHANNEL),
        vec![
            option("seconds", CommandOptionValue::Integer(0)),
            option(
                "reason",
                CommandOptionValue::String("disabled test".to_owned()),
            ),
        ],
    );
    runtime.on_interaction(&request).await;
    let reply = mock.deferred_reply();
    assert_eq!(reply["content"], two_bot_core::MODERATION_DISABLED_REPLY);
    assert_eq!(mock.requests().len(), 2, "only defer and edit");
    let outcome: String = sqlx::query_scalar("SELECT outcome FROM moderation_audit")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(outcome, "refused");
    mock.shutdown().await;
    db.close().await;
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
async fn unusable_success_receipt_retains_uncertainty_fence() {
    let db = TestDb::new().await;
    db.seed(Some("8888"), 120_000, 5).await;
    for (index, body) in [
        "{}",
        "not JSON",
        r#"{"id":null}"#,
        r#"{"id":0}"#,
        r#"{"id":""}"#,
        r#"{"id":"0"}"#,
        r#"{"id":"abc"}"#,
        // "+123" is covered by its own seeded test below: the executor
        // accepts it (`u64` parses a leading '+') while the runtime
        // all-digits guard rejects it, so only a fresh fence exercises
        // the Ok(_) unusable-receipt branch.
        r#"{"id":"18446744073709551616"}"#,
    ]
    .into_iter()
    .enumerate()
    {
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
        let (remembered, last_id, claim) = db.row().await.expect("row retained");
        assert_eq!(remembered, "remember this", "{body}");
        assert_eq!(last_id.as_deref(), Some("8888"), "{body}");
        assert!(
            claim.is_some(),
            "uncertain delivery keeps the fence for {body}"
        );
        // Only the first attempt reaches the wire; the fence holds the rest.
        assert_eq!(
            mock.posts_to("/messages").await.len(),
            usize::from(index == 0),
            "{body}"
        );
        assert!(mock.deletes().is_empty(), "no retirement for {body}");
        mock.shutdown().await;
    }
    assert_eq!(
        db.audits().await,
        vec![("sticky.run".to_owned(), "post_failed".to_owned())],
        "only the first attempt audits; fenced holds stay silent"
    );
    // The fence outlives the 60s claim lease: a probe past expiry is held,
    // and the next accepted message holds without posting a duplicate.
    assert!(
        store::claim_sticky_post(
            &db.pool,
            GUILD_S,
            CHANNEL_S,
            "lease-probe",
            now_millis_for_test() + 120_000,
        )
        .await
        .expect("lease probe")
        .is_none(),
        "uncertainty survives lease expiry"
    );
    let (runtime, mock) = db_runtime(&db, vec![]).await;
    assert_eq!(
        runtime
            .on_message(&message(25, CHANNEL, false, Some(GUILD)))
            .await,
        ActivityOutcome::Held
    );
    assert!(
        mock.posts_to("/messages").await.is_empty(),
        "fenced channel sends no duplicate"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn plus_prefixed_receipt_id_retains_uncertainty_fence() {
    // The executor accepts "+123" (`u64::from_str` allows a leading '+')
    // but the runtime all-digits guard routes it to the Ok(_) branch. A
    // fresh fence is required: sharing the loop above would hold before
    // the wire and never execute that branch.
    let db = TestDb::new().await;
    db.seed(Some("8888"), 120_000, 5).await;
    let (runtime, mock) = db_runtime_script(
        &db,
        vec![RestResponse {
            status: 200,
            body: Some(r#"{"id":"+123"}"#.into()),
            delay: Duration::ZERO,
        }],
    )
    .await;
    assert_eq!(
        runtime
            .on_message(&message(26, CHANNEL, false, Some(GUILD)))
            .await,
        ActivityOutcome::Held,
    );
    let (remembered, last_id, claim) = db.row().await.expect("row retained");
    assert_eq!(remembered, "remember this");
    assert_eq!(last_id.as_deref(), Some("8888"));
    assert!(claim.is_some(), "unusable receipt keeps the fence armed");
    assert_eq!(
        mock.posts_to("/messages").await.len(),
        1,
        "the POST reaches the wire before the receipt is judged unusable"
    );
    assert!(mock.deletes().is_empty(), "no retirement");
    assert_eq!(
        db.audits().await,
        vec![("sticky.run".to_owned(), "post_failed".to_owned())],
    );
    // The fence outlives the 60s claim lease: a probe past expiry is held,
    // and the next accepted message holds without posting a duplicate.
    // (Reverting the Ok(_) branch to release would turn the claim, probe
    // and follow-up assertions red.)
    assert!(
        store::claim_sticky_post(
            &db.pool,
            GUILD_S,
            CHANNEL_S,
            "plus-probe",
            now_millis_for_test() + 120_000,
        )
        .await
        .expect("lease probe")
        .is_none(),
        "uncertainty survives lease expiry"
    );
    mock.shutdown().await;
    let (runtime, mock) = db_runtime(&db, vec![]).await;
    assert_eq!(
        runtime
            .on_message(&message(27, CHANNEL, false, Some(GUILD)))
            .await,
        ActivityOutcome::Held
    );
    assert!(
        mock.posts_to("/messages").await.is_empty(),
        "fenced channel sends no duplicate"
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
async fn post_transport_failure_retains_uncertainty_fence() {
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
    assert!(claim.is_some(), "ambiguous 500 keeps the fence");

    // The fence holds the next activity: no duplicate reaches the wire.
    let outcome = runtime
        .on_message(&message(23, CHANNEL, false, Some(GUILD)))
        .await;
    assert_eq!(outcome, ActivityOutcome::Held);
    assert_eq!(
        mock.posts_to("/messages").await.len(),
        1,
        "failed attempt only; no retry while fenced"
    );
    assert_eq!(
        db.audits().await.len(),
        1,
        "fenced hold audits nothing more"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn obsolete_post_after_admin_update_preserves_admin_state() {
    let db = TestDb::new().await;
    db.seed(Some("8000000000000000010"), 120_000, 5).await;
    // Hold the accepted POST open so the administrator update lands between
    // the wire post and the store record. The mock delay is the hold; the
    // POST-arrival poll is the deterministic sync (no blind sleeps).
    let (runtime, mock) = db_runtime_script(
        &db,
        vec![RestResponse {
            status: 200,
            body: None,
            delay: Duration::from_millis(1000),
        }],
    )
    .await;
    let msg = message(30, CHANNEL, false, Some(GUILD));
    let (outcome, _) = tokio::join!(runtime.on_message(&msg), async {
        wait_for(
            || {
                mock.requests()
                    .iter()
                    .any(|r| r.method == "POST" && r.path.ends_with("/messages"))
            },
            "replacement POST entered",
        )
        .await;
        // Independent administrator path: the upsert clears the claim but
        // preserves the live post columns.
        let updated = store::put_sticky(
            &db.pool,
            &PutSticky {
                guild_id: GUILD_S,
                channel_id: CHANNEL_S,
                body: "admin v2",
                debounce_seconds: 5,
                enabled: true,
                actor_id: "admin-9",
                now_ms: now_millis_for_test(),
            },
        )
        .await
        .expect("admin update");
        assert!(!updated, "seed row already existed");
    });
    assert_eq!(outcome, ActivityOutcome::Held, "lost claim reports held");

    // The orphan replacement is deleted exactly once; the previous sticky is
    // never retired through the lost-claim branch.
    let deletes = mock.deletes();
    assert_eq!(deletes.len(), 1, "one orphan cleanup: {deletes:?}");
    assert_eq!(
        deletes[0].path, "/api/v10/channels/3333/messages/9000000000000000000",
        "orphan id from the held POST",
    );
    assert!(
        !deletes[0].path.ends_with("8000000000000000010"),
        "previous sticky untouched",
    );

    // Administrator state wins: new body, previous post columns, no claim,
    // and no success audit from the obsolete completion.
    let (body, last_id, claim) = db.row().await.expect("row exists");
    assert_eq!(body, "admin v2");
    assert_eq!(
        last_id.as_deref(),
        Some("8000000000000000010"),
        "obsolete record cannot overwrite the live post",
    );
    assert_eq!(
        claim, None,
        "admin update cleared the claim; record recreates none"
    );
    assert!(
        db.audits().await.is_empty(),
        "lost claim writes no audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn obsolete_post_after_admin_update_with_failed_cleanup_rearms_fence() {
    let db = TestDb::new().await;
    db.seed(Some("8000000000000000013"), 120_000, 5).await;
    // Hold the accepted POST open so the administrator update lands between
    // the wire post and the store record; the orphan DELETE then fails
    // (403), leaving a possibly-live replacement that must re-arm the
    // fence. The script is consumed in request order: POST, then DELETE.
    let (runtime, mock) = db_runtime_script(
        &db,
        vec![
            RestResponse {
                status: 200,
                body: None,
                delay: Duration::from_millis(1000),
            },
            RestResponse::status(403),
        ],
    )
    .await;
    let msg = message(32, CHANNEL, false, Some(GUILD));
    let (outcome, _) = tokio::join!(runtime.on_message(&msg), async {
        wait_for(
            || {
                mock.requests()
                    .iter()
                    .any(|r| r.method == "POST" && r.path.ends_with("/messages"))
            },
            "replacement POST entered",
        )
        .await;
        // Independent administrator path: the upsert clears the claim and
        // disarms the fence but preserves the live post columns.
        let updated = store::put_sticky(
            &db.pool,
            &PutSticky {
                guild_id: GUILD_S,
                channel_id: CHANNEL_S,
                body: "admin v2",
                debounce_seconds: 5,
                enabled: true,
                actor_id: "admin-9",
                now_ms: now_millis_for_test(),
            },
        )
        .await
        .expect("admin update");
        assert!(!updated, "seed row already existed");
    });
    assert_eq!(outcome, ActivityOutcome::Held, "lost claim reports held");

    // The orphan replacement cleanup was attempted exactly once.
    let deletes = mock.deletes();
    assert_eq!(deletes.len(), 1, "one orphan cleanup: {deletes:?}");
    assert_eq!(
        deletes[0].path, "/api/v10/channels/3333/messages/9000000000000000000",
        "orphan id from the held POST",
    );

    // Administrator state wins on the row, but the unconfirmed cleanup
    // re-arms the fence: the claim stays cleared (the re-arm is
    // claim-blind) while delivery_uncertain blocks the next window.
    let (body, last_id, claim) = db.row().await.expect("row exists");
    assert_eq!(body, "admin v2");
    assert_eq!(
        last_id.as_deref(),
        Some("8000000000000000013"),
        "obsolete record cannot overwrite the live post",
    );
    assert_eq!(
        claim, None,
        "admin update cleared the claim; the re-arm adds none"
    );
    assert_eq!(
        db.audits().await,
        vec![("sticky.run".to_owned(), "post_failed".to_owned())],
        "unconfirmed cleanup audits failure, never success"
    );

    // The re-armed fence outlives the 60s claim lease: a probe past expiry
    // is held, and the next accepted message holds without posting a
    // duplicate. (Deleting the moved-on fence_uncertain call turns the
    // probe red and lets the follow-up duplicate.)
    assert!(
        store::claim_sticky_post(
            &db.pool,
            GUILD_S,
            CHANNEL_S,
            "rearm-probe",
            now_millis_for_test() + 120_000,
        )
        .await
        .expect("lease probe")
        .is_none(),
        "moved-on re-arm survives lease expiry"
    );
    mock.shutdown().await;
    let (runtime, mock) = db_runtime(&db, vec![]).await;
    assert_eq!(
        runtime
            .on_message(&message(33, CHANNEL, false, Some(GUILD)))
            .await,
        ActivityOutcome::Held
    );
    assert!(
        mock.posts_to("/messages").await.is_empty(),
        "fenced channel sends no duplicate"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn failed_record_with_admin_update_during_cleanup_rearms_fence() {
    let db = TestDb::new().await;
    db.seed(Some("8000000000000000017"), 120_000, 5).await;
    // Scoped completion-only fault, as in
    // completion_db_failure_audits_post_failed_and_preserves_newer_claim:
    // the trigger rejects only the record that carries this test's orphan
    // id. The claim and the administrator upsert pass through.
    sqlx::query(
        "CREATE OR REPLACE FUNCTION sticky_test_fail_orphan() RETURNS trigger AS $$
         BEGIN
           IF NEW.last_message_id = '9000000000000000000' THEN
             RAISE EXCEPTION 'scoped completion-only failure';
           END IF;
           RETURN NEW;
         END;
         $$ LANGUAGE plpgsql",
    )
    .execute(&db.pool)
    .await
    .expect("install fault function");
    sqlx::query("DROP TRIGGER IF EXISTS sticky_test_fail_orphan_trg ON sticky_messages")
        .execute(&db.pool)
        .await
        .expect("drop stale fault trigger");
    sqlx::query(
        "CREATE TRIGGER sticky_test_fail_orphan_trg
           BEFORE UPDATE ON sticky_messages FOR EACH ROW
           EXECUTE FUNCTION sticky_test_fail_orphan()",
    )
    .execute(&db.pool)
    .await
    .expect("install fault trigger");

    // Hold the orphan DELETE open so the administrator update lands between
    // the failed record and the failed cleanup (403): the row is disarmed
    // at diagnose time with a possibly-live orphan, the same state as an
    // update landing during the POST. The script is consumed in request
    // order: POST, then DELETE.
    let (runtime, mock) = db_runtime_script(
        &db,
        vec![
            RestResponse {
                status: 200,
                body: None,
                delay: Duration::ZERO,
            },
            RestResponse {
                status: 403,
                body: None,
                delay: Duration::from_millis(1000),
            },
        ],
    )
    .await;
    let msg = message(36, CHANNEL, false, Some(GUILD));
    let (outcome, _) = tokio::join!(runtime.on_message(&msg), async {
        wait_for(
            || {
                mock.requests()
                    .iter()
                    .any(|r| r.method == "DELETE" && r.path.contains("/messages/"))
            },
            "orphan DELETE entered",
        )
        .await;
        // Independent administrator path: the upsert clears the claim and
        // disarms the fence but preserves the live post columns.
        let updated = store::put_sticky(
            &db.pool,
            &PutSticky {
                guild_id: GUILD_S,
                channel_id: CHANNEL_S,
                body: "admin v3",
                debounce_seconds: 5,
                enabled: true,
                actor_id: "admin-9",
                now_ms: now_millis_for_test(),
            },
        )
        .await
        .expect("admin update");
        assert!(!updated, "seed row already existed");
    });
    assert_eq!(outcome, ActivityOutcome::Held, "failed record reports held");

    // The orphan replacement cleanup was attempted exactly once.
    let deletes = mock.deletes();
    assert_eq!(deletes.len(), 1, "one orphan cleanup: {deletes:?}");
    assert_eq!(
        deletes[0].path, "/api/v10/channels/3333/messages/9000000000000000000",
        "orphan id from the accepted POST",
    );

    // Administrator state wins on the row, but the unconfirmed cleanup
    // re-arms the fence: the claim stays cleared (the re-arm is
    // claim-blind) while delivery_uncertain blocks the next window.
    let (body, last_id, claim) = db.row().await.expect("row exists");
    assert_eq!(body, "admin v3");
    assert_eq!(
        last_id.as_deref(),
        Some("8000000000000000017"),
        "failed record cannot overwrite the live post",
    );
    assert_eq!(
        claim, None,
        "admin update cleared the claim; the re-arm adds none"
    );
    assert_eq!(
        db.audits().await,
        vec![("sticky.run".to_owned(), "post_failed".to_owned())],
        "unconfirmed cleanup audits failure, never success"
    );

    // The re-armed fence outlives the 60s claim lease: a probe past expiry
    // is held, and the next accepted message holds without posting a
    // duplicate. (Deleting the record-failed fence_uncertain call turns the
    // probe red and lets the follow-up duplicate.)
    assert!(
        store::claim_sticky_post(
            &db.pool,
            GUILD_S,
            CHANNEL_S,
            "rearm-probe",
            now_millis_for_test() + 120_000,
        )
        .await
        .expect("lease probe")
        .is_none(),
        "record-failed re-arm survives lease expiry"
    );
    mock.shutdown().await;
    let (runtime, mock) = db_runtime(&db, vec![]).await;
    assert_eq!(
        runtime
            .on_message(&message(37, CHANNEL, false, Some(GUILD)))
            .await,
        ActivityOutcome::Held
    );
    assert!(
        mock.posts_to("/messages").await.is_empty(),
        "fenced channel sends no duplicate"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn ambiguous_post_error_with_admin_update_during_post_rearms_fence() {
    let db = TestDb::new().await;
    db.seed(Some("8000000000000000021"), 120_000, 5).await;
    // Hold the ambiguous POST (500) open so the administrator update lands
    // mid-flight: the claim's fence is disarmed at diagnose time with a
    // possibly-live post, and only a claim-blind re-arm holds the window.
    let (runtime, mock) = db_runtime_script(
        &db,
        vec![RestResponse {
            status: 500,
            body: None,
            delay: Duration::from_millis(1000),
        }],
    )
    .await;
    let msg = message(40, CHANNEL, false, Some(GUILD));
    let (outcome, _) = tokio::join!(runtime.on_message(&msg), async {
        wait_for(
            || {
                mock.requests()
                    .iter()
                    .any(|r| r.method == "POST" && r.path.ends_with("/messages"))
            },
            "ambiguous POST entered",
        )
        .await;
        // Independent administrator path: the upsert clears the claim and
        // disarms the fence but preserves the live post columns.
        let updated = store::put_sticky(
            &db.pool,
            &PutSticky {
                guild_id: GUILD_S,
                channel_id: CHANNEL_S,
                body: "admin v4",
                debounce_seconds: 5,
                enabled: true,
                actor_id: "admin-9",
                now_ms: now_millis_for_test(),
            },
        )
        .await
        .expect("admin update");
        assert!(!updated, "seed row already existed");
    });
    assert_eq!(
        outcome,
        ActivityOutcome::Held,
        "ambiguous error reports held"
    );

    // The ambiguous attempt reached the wire exactly once; with no usable
    // receipt there is no orphan cleanup to attempt.
    assert_eq!(
        mock.posts_to("/messages").await.len(),
        1,
        "failed attempt only"
    );
    assert!(mock.deletes().is_empty(), "no orphan id, no cleanup");

    // Administrator state wins on the row, but the ambiguous outcome
    // re-arms the fence: the claim stays cleared (the re-arm is
    // claim-blind) while delivery_uncertain blocks the next window.
    let (body, last_id, claim) = db.row().await.expect("row exists");
    assert_eq!(body, "admin v4");
    assert_eq!(
        last_id.as_deref(),
        Some("8000000000000000021"),
        "ambiguous post records nothing",
    );
    assert_eq!(
        claim, None,
        "admin update cleared the claim; the re-arm adds none"
    );
    assert_eq!(
        db.audits().await,
        vec![("sticky.run".to_owned(), "post_failed".to_owned())],
        "ambiguous outcome audits failure, never success"
    );

    // The re-armed fence outlives the 60s claim lease: a probe past expiry
    // is held, and the next accepted message holds without posting a
    // duplicate. (Deleting the ambiguous-error fence_uncertain call turns
    // the probe red and lets the follow-up duplicate.)
    assert!(
        store::claim_sticky_post(
            &db.pool,
            GUILD_S,
            CHANNEL_S,
            "rearm-probe",
            now_millis_for_test() + 120_000,
        )
        .await
        .expect("lease probe")
        .is_none(),
        "ambiguous-error re-arm survives lease expiry"
    );
    mock.shutdown().await;
    let (runtime, mock) = db_runtime(&db, vec![]).await;
    assert_eq!(
        runtime
            .on_message(&message(41, CHANNEL, false, Some(GUILD)))
            .await,
        ActivityOutcome::Held
    );
    assert!(
        mock.posts_to("/messages").await.is_empty(),
        "fenced channel sends no duplicate"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn unusable_receipt_with_admin_update_during_post_rearms_fence() {
    let db = TestDb::new().await;
    db.seed(Some("8000000000000000022"), 120_000, 5).await;
    // Plus-prefixed receipt the executor accepts but the runtime
    // all-digits guard rejects, held open so the administrator update
    // lands mid-flight: the post may be live but can be neither recorded
    // nor deleted, and only a claim-blind re-arm holds the window.
    let (runtime, mock) = db_runtime_script(
        &db,
        vec![RestResponse {
            status: 200,
            body: Some(r#"{"id":"+123"}"#.into()),
            delay: Duration::from_millis(1000),
        }],
    )
    .await;
    let msg = message(42, CHANNEL, false, Some(GUILD));
    let (outcome, _) = tokio::join!(runtime.on_message(&msg), async {
        wait_for(
            || {
                mock.requests()
                    .iter()
                    .any(|r| r.method == "POST" && r.path.ends_with("/messages"))
            },
            "unusable-receipt POST entered",
        )
        .await;
        // Independent administrator path: the upsert clears the claim and
        // disarms the fence but preserves the live post columns.
        let updated = store::put_sticky(
            &db.pool,
            &PutSticky {
                guild_id: GUILD_S,
                channel_id: CHANNEL_S,
                body: "admin v5",
                debounce_seconds: 5,
                enabled: true,
                actor_id: "admin-9",
                now_ms: now_millis_for_test(),
            },
        )
        .await
        .expect("admin update");
        assert!(!updated, "seed row already existed");
    });
    assert_eq!(
        outcome,
        ActivityOutcome::Held,
        "unusable receipt reports held"
    );

    // The POST reached the wire before the receipt was judged unusable;
    // the unusable id leaves no orphan cleanup to attempt.
    assert_eq!(
        mock.posts_to("/messages").await.len(),
        1,
        "the POST reaches the wire before the receipt is judged unusable"
    );
    assert!(mock.deletes().is_empty(), "no orphan id, no cleanup");

    // Administrator state wins on the row, but the unusable receipt
    // re-arms the fence: the claim stays cleared (the re-arm is
    // claim-blind) while delivery_uncertain blocks the next window.
    let (body, last_id, claim) = db.row().await.expect("row exists");
    assert_eq!(body, "admin v5");
    assert_eq!(
        last_id.as_deref(),
        Some("8000000000000000022"),
        "unusable receipt records nothing",
    );
    assert_eq!(
        claim, None,
        "admin update cleared the claim; the re-arm adds none"
    );
    assert_eq!(
        db.audits().await,
        vec![("sticky.run".to_owned(), "post_failed".to_owned())],
        "unusable receipt audits failure, never success"
    );

    // The re-armed fence outlives the 60s claim lease: a probe past expiry
    // is held, and the next accepted message holds without posting a
    // duplicate. (Deleting the unusable-receipt fence_uncertain call turns
    // the probe red and lets the follow-up duplicate.)
    assert!(
        store::claim_sticky_post(
            &db.pool,
            GUILD_S,
            CHANNEL_S,
            "rearm-probe",
            now_millis_for_test() + 120_000,
        )
        .await
        .expect("lease probe")
        .is_none(),
        "unusable-receipt re-arm survives lease expiry"
    );
    mock.shutdown().await;
    let (runtime, mock) = db_runtime(&db, vec![]).await;
    assert_eq!(
        runtime
            .on_message(&message(43, CHANNEL, false, Some(GUILD)))
            .await,
        ActivityOutcome::Held
    );
    assert!(
        mock.posts_to("/messages").await.is_empty(),
        "fenced channel sends no duplicate"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn obsolete_post_after_admin_remove_preserves_absence() {
    let db = TestDb::new().await;
    db.seed(Some("8000000000000000011"), 120_000, 5).await;
    let (runtime, mock) = db_runtime_script(
        &db,
        vec![RestResponse {
            status: 200,
            body: None,
            delay: Duration::from_millis(1000),
        }],
    )
    .await;
    let msg = message(31, CHANNEL, false, Some(GUILD));
    let (outcome, _) = tokio::join!(runtime.on_message(&msg), async {
        wait_for(
            || {
                mock.requests()
                    .iter()
                    .any(|r| r.method == "POST" && r.path.ends_with("/messages"))
            },
            "replacement POST entered",
        )
        .await;
        let removed = store::delete_sticky(&db.pool, GUILD_S, CHANNEL_S)
            .await
            .expect("admin remove");
        assert!(
            matches!(removed, two_bot_core::sticky::RemoveOutcome::Removed { .. }),
            "seed row removed",
        );
    });
    assert_eq!(outcome, ActivityOutcome::Held, "lost claim reports held");

    // Exactly the orphan goes; the removed previous id is never re-deleted
    // through this branch, and the row stays absent.
    let deletes = mock.deletes();
    assert_eq!(deletes.len(), 1, "one orphan cleanup: {deletes:?}");
    assert_eq!(
        deletes[0].path,
        "/api/v10/channels/3333/messages/9000000000000000000",
    );
    assert!(
        db.row().await.is_none(),
        "obsolete record cannot recreate administrator-removed state",
    );
    assert!(
        db.audits().await.is_empty(),
        "lost claim writes no audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn completion_db_failure_audits_post_failed_and_preserves_newer_claim() {
    let db = TestDb::new().await;
    db.seed(Some("8000000000000000012"), 120_000, 5).await;
    // Scoped completion-only fault: the trigger rejects only the record that
    // carries this test's orphan id. The claim (no post columns) and the
    // release (post columns untouched) pass through.
    sqlx::query(
        "CREATE OR REPLACE FUNCTION sticky_test_fail_orphan() RETURNS trigger AS $$
         BEGIN
           IF NEW.last_message_id = '9000000000000000000' THEN
             RAISE EXCEPTION 'scoped completion-only failure';
           END IF;
           RETURN NEW;
         END;
         $$ LANGUAGE plpgsql",
    )
    .execute(&db.pool)
    .await
    .expect("install fault function");
    sqlx::query("DROP TRIGGER IF EXISTS sticky_test_fail_orphan_trg ON sticky_messages")
        .execute(&db.pool)
        .await
        .expect("drop stale fault trigger");
    sqlx::query(
        "CREATE TRIGGER sticky_test_fail_orphan_trg
           BEFORE UPDATE ON sticky_messages FOR EACH ROW
           EXECUTE FUNCTION sticky_test_fail_orphan()",
    )
    .execute(&db.pool)
    .await
    .expect("install fault trigger");

    let (runtime, mock) = db_runtime(&db, Vec::new()).await;
    let outcome = runtime
        .on_message(&message(32, CHANNEL, false, Some(GUILD)))
        .await;
    assert_eq!(outcome, ActivityOutcome::Held);
    assert_eq!(
        db.audits().await,
        vec![("sticky.run".to_owned(), "post_failed".to_owned())],
    );
    let deletes = mock.deletes();
    assert_eq!(deletes.len(), 1, "orphan cleaned up: {deletes:?}");
    assert_eq!(
        deletes[0].path,
        "/api/v10/channels/3333/messages/9000000000000000000",
    );
    let (body, last_id, claim) = db.row().await.expect("row exists");
    assert_eq!(body, "remember this");
    assert_eq!(
        last_id.as_deref(),
        Some("8000000000000000012"),
        "failed record leaves the live post",
    );
    assert_eq!(claim, None, "own claim released for the next activity");

    sqlx::query("DROP TRIGGER IF EXISTS sticky_test_fail_orphan_trg ON sticky_messages")
        .execute(&db.pool)
        .await
        .expect("remove fault trigger");
    sqlx::query("DROP FUNCTION IF EXISTS sticky_test_fail_orphan()")
        .execute(&db.pool)
        .await
        .expect("remove fault function");

    // Claim ownership: releasing a stale unrelated token cannot clear a newer
    // claim — the property the post_failed rollback relies on.
    let grant = store::claim_sticky_post(
        &db.pool,
        GUILD_S,
        CHANNEL_S,
        "newer-claim",
        now_millis_for_test(),
    )
    .await
    .expect("newer claim")
    .expect("window free after release");
    assert_eq!(grant.body, "remember this");
    store::release_sticky_post(&db.pool, GUILD_S, CHANNEL_S, "stale-unrelated")
        .await
        .expect("stale release");
    let (_, _, claim) = db.row().await.expect("row exists");
    assert_eq!(
        claim.as_deref(),
        Some("newer-claim"),
        "stale release preserves the newer claim",
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

// --- acceptance: sticky store-failure refusals audit like rejections ------
//
// Locked contract: every sticky store-failure refusal (the `put_sticky`
// failure in `/sticky` and the `delete_sticky` failure in `/sticky-remove`)
// writes exactly one `automation_audit_log` row per refusal whenever the
// pool accepts it, in the existing refusal shape: the attempted `sticky.*`
// action (`sticky.create` when no row exists yet, `sticky.update` when one
// does — the same existence read the validation rejection already uses to
// name its action — and `sticky.delete` for a remove), outcome `rejected`,
// the invoking channel as `target_key`, the invoking member as `actor_id`,
// and a bounded single-token `reason` (a fixed error class such as the log
// line's `db_class` token, or an equivalent fixed token — never the raw
// sqlx / Postgres message, which can carry statement or fixture detail).
// The user reply stays the existing fixed store-failure text. A dead pool
// (audit write itself impossible) degrades to that same safe reply with no
// panic and no leaked internals. Every other sticky path keeps its existing
// audit behavior.

const STICKY_STORE_FAILURE_REPLY: &str = "Sticky command failed; try again.";

/// Fail only writes to `sticky_messages`: reads still succeed, so the
/// refusal comes from the upsert itself while `automation_audit_log` stays
/// writable and an audit attempt on the live pool can still land.
async fn fail_sticky_writes(db: &TestDb) {
    sqlx::query(
        "CREATE OR REPLACE FUNCTION fail_sticky_write() RETURNS trigger AS $$
         BEGIN
           RAISE EXCEPTION 'induced sticky write failure';
         END;
         $$ LANGUAGE plpgsql",
    )
    .execute(&db.pool)
    .await
    .expect("install write fault function");
    sqlx::query(
        "CREATE TRIGGER fail_sticky_write_trg
           BEFORE INSERT OR UPDATE ON sticky_messages FOR EACH ROW
           EXECUTE FUNCTION fail_sticky_write()",
    )
    .execute(&db.pool)
    .await
    .expect("install write fault trigger");
}

/// Fail only DELETEs from `sticky_messages`: the row stays readable, so the
/// refusal comes from the delete itself.
async fn fail_sticky_deletes(db: &TestDb) {
    sqlx::query(
        "CREATE OR REPLACE FUNCTION fail_sticky_delete() RETURNS trigger AS $$
         BEGIN
           RAISE EXCEPTION 'induced sticky delete failure';
         END;
         $$ LANGUAGE plpgsql",
    )
    .execute(&db.pool)
    .await
    .expect("install delete fault function");
    sqlx::query(
        "CREATE TRIGGER fail_sticky_delete_trg
           BEFORE DELETE ON sticky_messages FOR EACH ROW
           EXECUTE FUNCTION fail_sticky_delete()",
    )
    .execute(&db.pool)
    .await
    .expect("install delete fault trigger");
}

/// The refusal shape every sticky store-failure audit row must share.
fn assert_sticky_store_failure_audit(
    action: &str,
    target_key: Option<&str>,
    outcome: &str,
    reason: Option<&str>,
    actor_id: Option<&str>,
    expected_action: &str,
) {
    assert_eq!(action, expected_action);
    assert_eq!(
        target_key,
        Some(CHANNEL_S),
        "audit names the invoking channel, like every other sticky row"
    );
    assert_eq!(outcome, "rejected");
    assert_eq!(
        actor_id,
        Some("88"),
        "audit names the invoking member, like a validation rejection"
    );
    let reason = reason.expect("store-failure refusal audits a reason");
    assert!(
        !reason.is_empty() && reason.len() <= 64 && !reason.chars().any(char::is_whitespace),
        "reason is one bounded token, not raw error text: {reason:?}"
    );
    let lowered = reason.to_ascii_lowercase();
    for leaked in ["sqlx", "postgres", "sticky_messages", "induced"] {
        assert!(
            !lowered.contains(leaked),
            "reason must not leak {leaked}: {reason:?}"
        );
    }
}

/// Every completion-edit `content`, in invocation order.
fn sticky_store_failure_replies(mock: &MockRest, invocations: usize) -> Vec<String> {
    let edits: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH")
        .collect();
    assert_eq!(edits.len(), invocations, "one completion per invocation");
    edits
        .iter()
        .map(|edit| {
            let json: serde_json::Value =
                serde_json::from_slice(&edit.body).expect("completion json");
            json["content"]
                .as_str()
                .expect("reply content is text")
                .to_owned()
        })
        .collect()
}

fn sticky_set_interaction() -> Interaction {
    slash(
        "sticky",
        Some(CHANNEL),
        vec![option(
            "body",
            CommandOptionValue::String("pin me".to_owned()),
        )],
    )
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_set_create_store_failure_audits_one_rejected_row_per_refusal() {
    let db = TestDb::new().await;
    fail_sticky_writes(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime.on_interaction(&sticky_set_interaction()).await;
    runtime.on_interaction(&sticky_set_interaction()).await;

    assert_eq!(
        sticky_store_failure_replies(&mock, 2),
        vec![
            STICKY_STORE_FAILURE_REPLY.to_owned(),
            STICKY_STORE_FAILURE_REPLY.to_owned()
        ],
        "write failures keep the existing safe reply text"
    );
    assert!(
        db.row().await.is_none(),
        "failed writes store no sticky row"
    );
    let audits = schedule_audit_rows(&db).await;
    assert_eq!(
        audits.len(),
        2,
        "each create failure writes exactly one audit row: {audits:?}"
    );
    for (action, target_key, outcome, reason, actor_id) in &audits {
        assert_sticky_store_failure_audit(
            action,
            target_key.as_deref(),
            outcome,
            reason.as_deref(),
            actor_id.as_deref(),
            "sticky.create",
        );
    }
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_set_update_store_failure_audits_one_rejected_row() {
    let db = TestDb::new().await;
    db.seed(Some("8000000000000000005"), 120_000, 5).await;
    fail_sticky_writes(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime.on_interaction(&sticky_set_interaction()).await;

    assert_eq!(mock.deferred_reply()["content"], STICKY_STORE_FAILURE_REPLY);
    let (body, _, _) = db.row().await.expect("seeded row survives the refusal");
    assert_eq!(body, "remember this", "failed update changes no sticky row");
    let audits = schedule_audit_rows(&db).await;
    assert_eq!(
        audits.len(),
        1,
        "one update failure writes exactly one audit row: {audits:?}"
    );
    let (action, target_key, outcome, reason, actor_id) = &audits[0];
    assert_sticky_store_failure_audit(
        action,
        target_key.as_deref(),
        outcome,
        reason.as_deref(),
        actor_id.as_deref(),
        "sticky.update",
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn sticky_remove_store_failure_audits_one_rejected_row() {
    let db = TestDb::new().await;
    db.seed(Some("8000000000000000005"), 120_000, 5).await;
    fail_sticky_deletes(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash("sticky-remove", Some(CHANNEL), Vec::new()))
        .await;

    assert_eq!(mock.deferred_reply()["content"], STICKY_STORE_FAILURE_REPLY);
    assert!(
        db.row().await.is_some(),
        "failed delete keeps the sticky row"
    );
    assert!(
        mock.deletes().is_empty(),
        "no Discord message delete on a store refusal"
    );
    let audits = schedule_audit_rows(&db).await;
    assert_eq!(
        audits.len(),
        1,
        "one delete failure writes exactly one audit row: {audits:?}"
    );
    let (action, target_key, outcome, reason, actor_id) = &audits[0];
    assert_sticky_store_failure_audit(
        action,
        target_key.as_deref(),
        outcome,
        reason.as_deref(),
        actor_id.as_deref(),
        "sticky.delete",
    );
    mock.shutdown().await;
    db.close().await;
}

/// Dead-pool control: with a closed pool, the audit write is impossible by
/// construction, so every sticky command must still finish with the fixed
/// safe reply — no panic, no hang, no sqlx internals, pool address or
/// credentials in the user text. A closed pool fails every query
/// immediately; an unreachable pool would hold each command for the full
/// acquire timeout, so this control stays fast after the audit attempt
/// adds a second store call per refusal.
#[tokio::test]
async fn sticky_store_failures_on_dead_pool_reply_safely() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    pool.close().await;
    let runtime = CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(gates(true, true)),
        GUILD,
        true,
    );

    runtime.on_interaction(&sticky_set_interaction()).await;
    runtime
        .on_interaction(&slash("sticky-remove", Some(CHANNEL), Vec::new()))
        .await;

    let replies = sticky_store_failure_replies(&mock, 2);
    assert_eq!(
        replies,
        vec![STICKY_STORE_FAILURE_REPLY.to_owned(); 2],
        "dead pool degrades every sticky command to the safe reply"
    );
    for reply in &replies {
        for leaked in ["sqlx", "postgres", "127.0.0.1", "agent_test", "Pool"] {
            assert!(
                !reply.contains(leaked),
                "dead-pool reply must not leak {leaked}: {reply}"
            );
        }
    }
    mock.shutdown().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn automation_quota_refusals_reply_with_a_remedy_and_write_no_rows_or_audits() {
    use two_bot_core::automation_quota::AutomationQuota;

    let db = TestDb::new().await;
    for n in 0..25 {
        db.seed_schedule(GUILD_S, &format!("schedule-{n}")).await;
        db.seed_feed(GUILD_S, &format!("feed-{n}"), "rss").await;
    }
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;
    runtime
        .on_interaction(&slash(
            "schedule",
            Some(CHANNEL),
            vec![
                option("body", CommandOptionValue::String("hello".to_owned())),
                option("in-minutes", CommandOptionValue::Integer(30)),
            ],
        ))
        .await;
    assert_eq!(
        mock.deferred_reply()["content"],
        AutomationQuota::Schedules.to_string()
    );
    assert!(!mock
        .requests()
        .iter()
        .any(|request| request.path.starts_with("/api/v10/channels/")));
    mock.shutdown().await;

    let (runtime, mock) = db_runtime(&db, Vec::new()).await;
    runtime
        .on_interaction(&slash(
            "feed-add",
            Some(CHANNEL),
            vec![
                option("kind", CommandOptionValue::String("rss".to_owned())),
                option(
                    "source",
                    CommandOptionValue::String("https://example.com/new.xml".to_owned()),
                ),
            ],
        ))
        .await;
    assert_eq!(
        mock.deferred_reply()["content"],
        AutomationQuota::Feeds.to_string()
    );
    assert_eq!(db.schedule_rows().await.len(), 25);
    assert_eq!(db.feed_rows().await.len(), 25);
    // Schedule capacity refusal audits one `scheduled.create`/`rejected` row
    // (reason `quota`); feed capacity refusal still audits nothing.
    let schedule_audits = db.schedule_audits().await;
    assert_eq!(
        schedule_audits.len(),
        1,
        "schedule capacity refusal audits one row: {schedule_audits:?}"
    );
    assert_eq!(schedule_audits[0].0, "scheduled.create");
    assert_eq!(schedule_audits[0].2, "rejected");
    assert!(db.feed_audits().await.is_empty());
    assert!(!mock
        .requests()
        .iter()
        .any(|request| request.path.starts_with("/api/v10/channels/")));
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

// ── Acceptance: removal serialized with delivery (command path) ──────────
// Protected acceptance tests. While a relay carries unresolved pending
// delivery evidence, `/feed-remove` must refuse with a fixed
// recovery-required reply, retain the relay and its exact ledger row, and
// audit a recovery outcome — never claim removal, report the id missing,
// or fall back to the generic store-failure "try again" reply.

async fn seed_feed_delivery(db: &TestDb, feed_id: &str, state: &str) {
    sqlx::query(
        "INSERT INTO feed_deliveries
           (feed_id, item_key, nonce, state, message_id, first_seen_at, delivered_at)
         VALUES ($1, 'item-0', $2, $3,
                 CASE WHEN $3 = 'delivered' THEN '9001' END,
                 clock_timestamp(),
                 CASE WHEN $3 = 'delivered' THEN clock_timestamp() END)",
    )
    .bind(feed_id)
    .bind(format!("nonce-{feed_id}-item-0"))
    .bind(state)
    .execute(&db.pool)
    .await
    .expect("seed feed delivery");
}

/// (feed_id, item_key, nonce, state) across every guild in the fixture.
async fn all_feed_delivery_rows(db: &TestDb) -> Vec<(String, String, String, String)> {
    sqlx::query_as(
        "SELECT feed_id, item_key, nonce, state FROM feed_deliveries
          ORDER BY feed_id, item_key",
    )
    .fetch_all(&db.pool)
    .await
    .expect("feed delivery rows")
}

async fn feed_remove_reply(db: &TestDb, id: &str) -> (String, MockRest) {
    let (runtime, mock) = db_runtime(db, Vec::new()).await;
    runtime
        .on_interaction(&slash(
            "feed-remove",
            Some(CHANNEL),
            vec![option("id", CommandOptionValue::String(id.to_owned()))],
        ))
        .await;
    let edits: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH")
        .collect();
    assert_eq!(edits.len(), 1, "one completion per remove");
    let body: serde_json::Value = serde_json::from_slice(&edits[0].body).unwrap();
    (
        body["content"].as_str().expect("reply content").to_owned(),
        mock,
    )
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_remove_with_pending_delivery_refuses_and_retains_evidence() {
    let db = TestDb::new().await;
    db.seed_feed(GUILD_S, "feed-pending", "rss").await;
    seed_feed_delivery(&db, "feed-pending", "pending").await;
    let before = all_feed_delivery_rows(&db).await;
    assert_eq!(before.len(), 1);

    let (reply, mock) = feed_remove_reply(&db, "feed-pending").await;

    assert_ne!(reply, "Feed relay removed.", "must not claim removal");
    assert_ne!(reply, "No feed relay with that id.", "the relay exists");
    assert_ne!(
        reply, "Feed command failed; try again.",
        "a blind retry instruction cannot resolve unconfirmed send evidence"
    );
    assert!(
        reply.to_lowercase().contains("recover"),
        "fixed recovery-required reply, got: {reply}"
    );
    assert_eq!(
        db.feed_rows()
            .await
            .iter()
            .map(|row| row.0.as_str())
            .collect::<Vec<_>>(),
        vec!["feed-pending"],
        "the relay is retained"
    );
    assert_eq!(
        all_feed_delivery_rows(&db).await,
        before,
        "the exact pending ledger row is retained, not cascaded away"
    );
    let audits = db.feed_audits().await;
    assert_eq!(audits.len(), 1, "exactly one remove audit row");
    assert_eq!(audits[0].0, "feed.remove");
    assert_eq!(audits[0].1, "feed-pending");
    assert!(
        audits[0].2.contains("recover"),
        "fixed recovery-required audit outcome, got: {}",
        audits[0].2
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_remove_with_only_delivered_delivery_still_removes() {
    let db = TestDb::new().await;
    db.seed_feed(GUILD_S, "feed-settled", "rss").await;
    seed_feed_delivery(&db, "feed-settled", "delivered").await;

    let (reply, mock) = feed_remove_reply(&db, "feed-settled").await;

    assert_eq!(reply, "Feed relay removed.");
    assert!(db.feed_rows().await.is_empty(), "settled relay is deleted");
    assert!(
        all_feed_delivery_rows(&db).await.is_empty(),
        "settled ledger keeps its current cascade"
    );
    assert_eq!(
        db.feed_audits().await,
        vec![(
            "feed.remove".to_owned(),
            "feed-settled".to_owned(),
            "removed".to_owned()
        )]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_remove_foreign_id_reports_missing_and_retains_foreign_pending() {
    let db = TestDb::new().await;
    db.seed_feed("9999", "feed-foreign", "rss").await;
    seed_feed_delivery(&db, "feed-foreign", "pending").await;
    let before = all_feed_delivery_rows(&db).await;

    let (reply, mock) = feed_remove_reply(&db, "feed-foreign").await;

    assert_eq!(
        reply, "No feed relay with that id.",
        "a foreign id is indistinguishable from missing in this guild"
    );
    let foreign: i64 =
        sqlx::query_scalar("SELECT count(*) FROM feed_relays WHERE guild_id = '9999'")
            .fetch_one(&db.pool)
            .await
            .expect("foreign relay count");
    assert_eq!(foreign, 1, "the foreign relay is untouched");
    assert_eq!(
        all_feed_delivery_rows(&db).await,
        before,
        "the foreign pending evidence is untouched"
    );
    assert_eq!(
        db.feed_audits().await,
        vec![(
            "feed.remove".to_owned(),
            "feed-foreign".to_owned(),
            "missing".to_owned()
        )]
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
async fn feed_list_at_capacity_keeps_every_configured_guild_id_discoverable() {
    let db = TestDb::new().await;
    // Real relay ids are 32 hex characters; seed the full supported
    // capacity of 25 for the configured guild plus one foreign relay.
    let own_ids: Vec<String> = (1..=25).map(|index| format!("{index:032x}")).collect();
    for (position, id) in own_ids.iter().enumerate() {
        let kind = ["rss", "youtube", "twitch"][position % 3];
        db.seed_feed(GUILD_S, id, kind).await;
    }
    let foreign_id = "f".repeat(32);
    db.seed_feed("9999", &foreign_id, "rss").await;
    let rows_before = db.feed_rows().await;
    assert_eq!(rows_before.len(), 25, "seeded capacity");
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    // Whole rows page instead of squeezing sources: the default reply is
    // page 1 with navigation, and every id stays reachable exactly once
    // across the reachable pages.
    runtime
        .on_interaction(&slash("feed-list", Some(CHANNEL), Vec::new()))
        .await;
    let first = patch_contents(&mock)[0].clone();
    let total: usize = first
        .split("Page 1 of ")
        .nth(1)
        .expect("default reply announces its total")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("numeric total");
    assert!((2..=25).contains(&total), "capacity pages: {first}");
    for page in 2..=total as i64 {
        runtime
            .on_interaction(&slash(
                "feed-list",
                Some(CHANNEL),
                vec![option("page", CommandOptionValue::Integer(page))],
            ))
            .await;
    }

    // One ephemeral defer (flags 64) plus one mention-safe completion
    // (allowed_mentions parse []) per invocation, no extra calls.
    let requests = mock.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method == "POST" && r.path.ends_with("/callback"))
            .count(),
        total,
        "one defer per page"
    );
    let contents = patch_contents(&mock);
    assert_eq!(contents.len(), total, "one completion per page");
    let mut seen = Vec::new();
    for (index, content) in contents.iter().enumerate() {
        let page = index + 1;
        assert!(
            content.encode_utf16().count() <= 2000,
            "page {page} exceeds the 2,000 UTF-16 unit limit ({} units): {content}",
            content.encode_utf16().count()
        );
        assert!(
            content.contains(&format!("Page {page} of {total}")),
            "page {page} identifies itself: {content}"
        );
        assert!(
            content.contains("/feed-list") && content.contains("page"),
            "page {page} gives usable navigation: {content}"
        );
        assert!(
            !content.contains(&foreign_id),
            "other guild's relay never listed: {content}"
        );
        seen.extend(
            content
                .lines()
                .filter_map(|line| line.strip_prefix('`')?.split_once('`').map(|(id, _)| id))
                .map(str::to_owned),
        );
    }
    assert_eq!(
        seen, own_ids,
        "every configured-guild id reachable exactly once, in stable order"
    );
    assert_eq!(
        db.feed_rows().await,
        rows_before,
        "list leaves feed configuration unchanged"
    );
    assert!(
        db.feed_audits().await.is_empty(),
        "list writes no audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

/// Completion bodies for every invocation so far, pinning mention safety
/// (`allowed_mentions` parse `[]`) on each PATCH the way `deferred_reply`
/// does for the single-completion case.
fn patch_contents(mock: &MockRest) -> Vec<String> {
    mock.requests()
        .iter()
        .filter(|r| r.method == "PATCH")
        .map(|r| {
            let json: serde_json::Value = serde_json::from_slice(&r.body).expect("completion json");
            assert_eq!(
                json["allowed_mentions"]["parse"],
                serde_json::json!([]),
                "mention-safe completion"
            );
            json["content"]
                .as_str()
                .expect("completion content")
                .to_owned()
        })
        .collect()
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

// --- acceptance: feed store-failure refusals audit like other refusals ----
//
// Locked contract: every feed-add / feed-remove store-failure refusal
// writes exactly one `announcements_audit_log` row per refusal whenever the
// pool accepts it, in the existing refusal shape: the attempted command's
// `feed.*` action (`feed.create` for add, `feed.remove` for remove),
// outcome `rejected`, the invoking member as `actor_id`, and a bounded
// single-token `reason` (a fixed error class — never the raw sqlx /
// Postgres message, which can carry statement or fixture detail).
// `target_key` names the attempted relay: the fresh attempt id for adds,
// the user-supplied id for removes. The user reply stays the existing
// fixed store-failure text. Feed-list and feed-inspect stay unaudited even
// on failure (legacy list parity): a failed read replies with the same safe
// text and writes no audit row. A dead pool (the audit write itself
// impossible)
// degrades to a warn log plus that same safe reply with no panic and no
// leaked internals.

const FEED_STORE_FAILURE_REPLY: &str = "Feed command failed; try again.";

/// (action, target_key, outcome, reason, actor_id) for this guild's rows in
/// the shared announcements audit log.
async fn feed_audit_rows(
    db: &TestDb,
) -> Vec<(
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
)> {
    sqlx::query_as(
        "SELECT action, target_key, outcome, reason, actor_id
           FROM announcements_audit_log
          WHERE guild_id = $1 ORDER BY created_at, id",
    )
    .bind(GUILD_S)
    .fetch_all(&db.pool)
    .await
    .expect("announcement audit rows with reasons")
}

/// Drop the feed table inside this test's isolated schema: every store call
/// against it fails while `announcements_audit_log` stays writable, so an
/// audit attempt on the live pool can still land. CASCADE covers the
/// feed_deliveries.feed_id foreign key (0180_feeds.sql).
async fn drop_feed_relays(db: &TestDb) {
    sqlx::query("DROP TABLE feed_relays CASCADE")
        .execute(&db.pool)
        .await
        .expect("drop feed_relays");
}

/// Fail only INSERTs into `feed_relays`: any read on the add path still
/// succeeds, so the refusal comes from the write itself.
async fn fail_feed_inserts(db: &TestDb) {
    sqlx::query(
        "CREATE OR REPLACE FUNCTION fail_feed_insert() RETURNS trigger AS $$
         BEGIN
           RAISE EXCEPTION 'induced feed insert failure';
         END;
         $$ LANGUAGE plpgsql",
    )
    .execute(&db.pool)
    .await
    .expect("install insert fault function");
    sqlx::query(
        "CREATE TRIGGER fail_feed_insert_trg
           BEFORE INSERT ON feed_relays FOR EACH ROW
           EXECUTE FUNCTION fail_feed_insert()",
    )
    .execute(&db.pool)
    .await
    .expect("install insert fault trigger");
}

/// Fail only DELETEs from `feed_relays`, so the remove refusal comes from
/// the delete itself.
async fn fail_feed_deletes(db: &TestDb) {
    sqlx::query(
        "CREATE OR REPLACE FUNCTION fail_feed_delete() RETURNS trigger AS $$
         BEGIN
           RAISE EXCEPTION 'induced feed delete failure';
         END;
         $$ LANGUAGE plpgsql",
    )
    .execute(&db.pool)
    .await
    .expect("install delete fault function");
    sqlx::query(
        "CREATE TRIGGER fail_feed_delete_trg
           BEFORE DELETE ON feed_relays FOR EACH ROW
           EXECUTE FUNCTION fail_feed_delete()",
    )
    .execute(&db.pool)
    .await
    .expect("install delete fault trigger");
}

/// The refusal shape every feed store-failure audit row must share.
fn assert_feed_store_failure_audit(
    action: &str,
    outcome: &str,
    reason: Option<&str>,
    actor_id: Option<&str>,
    expected_action: &str,
) {
    assert_eq!(action, expected_action);
    assert_eq!(outcome, "rejected");
    assert_eq!(
        actor_id,
        Some("88"),
        "audit names the invoking member, like a validation rejection"
    );
    let reason = reason.expect("store-failure refusal audits a reason");
    assert!(
        !reason.is_empty() && reason.len() <= 64 && !reason.chars().any(char::is_whitespace),
        "reason is one bounded token, not raw error text: {reason:?}"
    );
    let lowered = reason.to_ascii_lowercase();
    for leaked in ["sqlx", "postgres", "feed_relays", "induced"] {
        assert!(
            !lowered.contains(leaked),
            "reason must not leak {leaked}: {reason:?}"
        );
    }
}

/// Every completion-edit `content`, in invocation order.
fn feed_store_failure_replies(mock: &MockRest, invocations: usize) -> Vec<String> {
    let edits: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH")
        .collect();
    assert_eq!(edits.len(), invocations, "one completion per invocation");
    edits
        .iter()
        .map(|edit| {
            let json: serde_json::Value =
                serde_json::from_slice(&edit.body).expect("completion json");
            json["content"]
                .as_str()
                .expect("reply content is text")
                .to_owned()
        })
        .collect()
}

fn feed_add_interaction() -> Interaction {
    slash(
        "feed-add",
        Some(CHANNEL),
        vec![
            option("kind", CommandOptionValue::String("rss".to_owned())),
            option(
                "source",
                CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
            ),
        ],
    )
}

fn feed_remove_interaction(id: &str) -> Interaction {
    slash(
        "feed-remove",
        Some(CHANNEL),
        vec![option("id", CommandOptionValue::String(id.to_owned()))],
    )
}

fn feed_inspect_interaction(id: &str) -> Interaction {
    slash(
        "feed-inspect",
        Some(CHANNEL),
        vec![option("id", CommandOptionValue::String(id.to_owned()))],
    )
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_add_store_failure_audits_one_rejected_row() {
    let db = TestDb::new().await;
    drop_feed_relays(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime.on_interaction(&feed_add_interaction()).await;

    assert_eq!(mock.deferred_reply()["content"], FEED_STORE_FAILURE_REPLY);
    let audits = feed_audit_rows(&db).await;
    assert_eq!(
        audits.len(),
        1,
        "one add store failure writes exactly one audit row: {audits:?}"
    );
    let (action, target_key, outcome, reason, actor_id) = &audits[0];
    assert_feed_store_failure_audit(
        action,
        outcome,
        reason.as_deref(),
        actor_id.as_deref(),
        "feed.create",
    );
    let target = target_key
        .as_deref()
        .expect("audit names the attempted feed id");
    assert!(!target.is_empty(), "target is the fresh attempt id");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_add_insert_store_failure_audits_one_row_per_refusal() {
    let db = TestDb::new().await;
    fail_feed_inserts(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime.on_interaction(&feed_add_interaction()).await;
    runtime.on_interaction(&feed_add_interaction()).await;

    assert_eq!(
        feed_store_failure_replies(&mock, 2),
        vec![
            FEED_STORE_FAILURE_REPLY.to_owned(),
            FEED_STORE_FAILURE_REPLY.to_owned()
        ],
        "write failures keep the existing safe reply text"
    );
    assert!(
        db.feed_rows().await.is_empty(),
        "failed writes store no feed row"
    );
    let audits = feed_audit_rows(&db).await;
    assert_eq!(
        audits.len(),
        2,
        "each write failure writes exactly one audit row: {audits:?}"
    );
    for (action, _, outcome, reason, actor_id) in &audits {
        assert_feed_store_failure_audit(
            action,
            outcome,
            reason.as_deref(),
            actor_id.as_deref(),
            "feed.create",
        );
    }
    let targets: std::collections::BTreeSet<Option<String>> = audits
        .iter()
        .map(|(_, target_key, _, _, _)| target_key.clone())
        .collect();
    assert_eq!(
        targets.len(),
        2,
        "each refusal audits its own fresh attempt id: {audits:?}"
    );
    assert!(
        targets
            .iter()
            .all(|target| target.as_deref().is_some_and(|t| !t.is_empty())),
        "every target names an attempted feed id: {audits:?}"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_remove_store_failure_audits_one_rejected_row() {
    let db = TestDb::new().await;
    drop_feed_relays(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_remove_interaction("feed-xyz"))
        .await;

    assert_eq!(mock.deferred_reply()["content"], FEED_STORE_FAILURE_REPLY);
    let audits = feed_audit_rows(&db).await;
    assert_eq!(
        audits.len(),
        1,
        "one remove store failure writes exactly one audit row: {audits:?}"
    );
    let (action, target_key, outcome, reason, actor_id) = &audits[0];
    assert_feed_store_failure_audit(
        action,
        outcome,
        reason.as_deref(),
        actor_id.as_deref(),
        "feed.remove",
    );
    assert_eq!(
        target_key.as_deref(),
        Some("feed-xyz"),
        "remove refusal names the user-supplied feed id"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_remove_delete_store_failure_audits_one_rejected_row() {
    let db = TestDb::new().await;
    db.seed_feed(GUILD_S, "feed-drop", "rss").await;
    fail_feed_deletes(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_remove_interaction("feed-drop"))
        .await;

    assert_eq!(mock.deferred_reply()["content"], FEED_STORE_FAILURE_REPLY);
    assert_eq!(
        db.feed_rows()
            .await
            .into_iter()
            .map(|(id, ..)| id)
            .collect::<Vec<_>>(),
        vec!["feed-drop".to_owned()],
        "failed delete leaves the feed row in place"
    );
    let audits = feed_audit_rows(&db).await;
    assert_eq!(
        audits.len(),
        1,
        "one delete failure writes exactly one audit row: {audits:?}"
    );
    let (action, target_key, outcome, reason, actor_id) = &audits[0];
    assert_feed_store_failure_audit(
        action,
        outcome,
        reason.as_deref(),
        actor_id.as_deref(),
        "feed.remove",
    );
    assert_eq!(
        target_key.as_deref(),
        Some("feed-drop"),
        "delete refusal names the attempted feed id"
    );
    mock.shutdown().await;
    db.close().await;
}

/// Legacy parity: feed-list is read-only and stays unaudited even when its
/// read fails — the same safe reply, and still no audit row.
#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_list_store_failure_writes_no_audit_row() {
    let db = TestDb::new().await;
    drop_feed_relays(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash("feed-list", Some(CHANNEL), Vec::new()))
        .await;

    assert_eq!(mock.deferred_reply()["content"], FEED_STORE_FAILURE_REPLY);
    assert!(
        feed_audit_rows(&db).await.is_empty(),
        "a failed list writes no audit row (legacy parity)"
    );
    assert!(
        db.feed_audits().await.is_empty(),
        "a failed list writes no feed audit row (legacy parity)"
    );
    mock.shutdown().await;
    db.close().await;
}

/// Inspect is a pure guild-scoped read: the saved relay renders with exact
/// id, kind, destination, enabled state, safe source and last attempt, while
/// configuration and audit stay untouched. `deferred_reply` pins the
/// ephemeral defer (type 5, flags 64) plus the mention-safe completion.
#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_inspect_shows_saved_relay_detail_without_mutation() {
    let db = TestDb::new().await;
    db.seed_feed(GUILD_S, "feed-own", "rss").await;
    db.seed_feed("9999", "feed-foreign", "twitch").await;
    let rows_before = db.feed_rows().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_inspect_interaction("feed-own"))
        .await;

    let json = mock.deferred_reply();
    let content = json["content"].as_str().expect("inspect content");
    assert!(content.contains("`feed-own`"), "exact id: {content}");
    assert!(content.contains("rss"), "kind stays visible: {content}");
    assert!(
        content.contains(&format!("<#{CHANNEL_S}>")),
        "destination stays visible: {content}"
    );
    assert!(
        content.contains("enabled"),
        "enabled state stays visible: {content}"
    );
    assert!(
        content.contains("example.com"),
        "safe source host stays visible: {content}"
    );
    assert!(
        content.to_ascii_lowercase().contains("attempt"),
        "last attempt stays visible: {content}"
    );
    assert!(
        !content.contains("feed-foreign"),
        "other guild's relay never leaks: {content}"
    );
    assert_eq!(
        db.feed_rows().await,
        rows_before,
        "inspect leaves feed configuration unchanged"
    );
    assert!(
        db.feed_audits().await.is_empty(),
        "inspect writes no audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

/// Absent and foreign-guild ids share one refusal: the guild-scoped read
/// finds nothing either way, and the copy must not oracle foreign relays.
/// Invalid ids and store failures refuse distinctly (see below).
#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_inspect_absent_and_foreign_ids_refuse_without_mutation() {
    let db = TestDb::new().await;
    db.seed_feed(GUILD_S, "feed-own", "rss").await;
    db.seed_feed("9999", "feed-foreign", "twitch").await;
    let rows_before = db.feed_rows().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    for id in ["feed-missing", "feed-foreign"] {
        runtime.on_interaction(&feed_inspect_interaction(id)).await;
    }

    let contents = patch_contents(&mock);
    assert_eq!(contents.len(), 2, "one completion per invocation");
    for content in &contents {
        assert_eq!(content, "No feed relay with that id.");
    }
    assert_eq!(
        db.feed_rows().await,
        rows_before,
        "refused inspect leaves feed configuration unchanged"
    );
    assert!(
        db.feed_audits().await.is_empty(),
        "refused inspect writes no audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

/// Invalid ids refuse at plan time, before any store read: with the table
/// dropped, every read would fail, so the invalid-id copy proves no read.
#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_inspect_invalid_id_refuses_before_any_store_read() {
    let db = TestDb::new().await;
    drop_feed_relays(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_inspect_interaction("bad id"))
        .await;

    assert_eq!(mock.deferred_reply()["content"], "Invalid feed id.");
    assert!(
        db.feed_audits().await.is_empty(),
        "an invalid inspect writes no audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

/// Inspect stays unaudited even when its read fails — the same safe reply
/// as the other feed commands, and still no audit row.
#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_inspect_store_failure_writes_no_audit_row() {
    let db = TestDb::new().await;
    drop_feed_relays(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&feed_inspect_interaction("feed-own"))
        .await;

    assert_eq!(mock.deferred_reply()["content"], FEED_STORE_FAILURE_REPLY);
    assert!(
        feed_audit_rows(&db).await.is_empty(),
        "a failed inspect writes no audit row"
    );
    assert!(
        db.feed_audits().await.is_empty(),
        "a failed inspect writes no feed audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

/// Dead-pool control: with a closed pool, the audit write is impossible by
/// construction, so every feed command must still emit a warn for the
/// operator and finish with the fixed safe reply — no panic, no hang, no
/// sqlx internals, pool address or credentials in the user text. A closed
/// pool fails every query immediately; an unreachable pool would hold each
/// command for the full acquire timeout, so this control stays fast after
/// the audit attempt adds a second store call per refusal.
#[tokio::test]
async fn feed_store_failures_on_dead_pool_warn_and_reply_safely() {
    let capture = crate::tracing_capture::global();
    let log_start = capture.text().len();
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    pool.close().await;
    let runtime = CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(gates(true, true)),
        GUILD,
        true,
    );

    runtime.on_interaction(&feed_add_interaction()).await;
    runtime
        .on_interaction(&feed_remove_interaction("feed-xyz"))
        .await;
    runtime
        .on_interaction(&slash("feed-list", Some(CHANNEL), Vec::new()))
        .await;
    runtime
        .on_interaction(&feed_inspect_interaction("feed-xyz"))
        .await;

    let replies = feed_store_failure_replies(&mock, 4);
    assert_eq!(
        replies,
        vec![FEED_STORE_FAILURE_REPLY.to_owned(); 4],
        "dead pool degrades every feed command to the safe reply"
    );
    for reply in &replies {
        for leaked in ["sqlx", "postgres", "127.0.0.1", "agent_test", "Pool"] {
            assert!(
                !reply.contains(leaked),
                "dead-pool reply must not leak {leaked}: {reply}"
            );
        }
    }
    // The failure stays operator-visible: at least one warn-or-above log
    // line was emitted while these refusals degraded. Sibling tests share
    // the process-wide capture, so this asserts presence, never absence.
    let logged = capture.text().split_off(log_start);
    assert!(
        logged
            .lines()
            .any(|line| line.starts_with("WARN") || line.starts_with("ERROR")),
        "dead-pool refusals leave a warn for the operator: {logged}"
    );
    mock.shutdown().await;
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

// --- schedule slice: guild-scoped CRUD + automation_audit_log -------------

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_create_persists_one_shot_for_the_invoking_channel_and_audits() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash(
            "schedule",
            Some(CHANNEL),
            vec![
                option("body", CommandOptionValue::String("hello".to_owned())),
                option("in-minutes", CommandOptionValue::Integer(30)),
            ],
        ))
        .await;

    let rows = db.schedule_rows().await;
    assert_eq!(rows.len(), 1, "one scheduled row written");
    let (id, channel_id) = &rows[0];
    assert_eq!(channel_id, CHANNEL_S, "schedule binds the invoking channel");
    let interval: Option<i64> =
        sqlx::query_scalar("SELECT interval_seconds FROM scheduled_messages WHERE id = $1")
            .bind(id)
            .fetch_one(&db.pool)
            .await
            .expect("interval");
    assert_eq!(interval, None, "in-minutes is a one-shot");

    let json = mock.deferred_reply();
    assert!(
        json["content"]
            .as_str()
            .expect("confirm text")
            .starts_with(&format!("Scheduled message `{id}` at ")),
        "ephemeral confirmation names the new id: {}",
        json["content"]
    );
    assert_eq!(
        db.schedule_audits().await,
        vec![("scheduled.create".to_owned(), id.clone(), "ok".to_owned())]
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_create_recurring_sets_interval() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash(
            "schedule",
            Some(CHANNEL),
            vec![
                option("body", CommandOptionValue::String("hourly".to_owned())),
                option("every-minutes", CommandOptionValue::Integer(90)),
            ],
        ))
        .await;

    let rows = db.schedule_rows().await;
    assert_eq!(rows.len(), 1, "one scheduled row written");
    let interval: Option<i64> =
        sqlx::query_scalar("SELECT interval_seconds FROM scheduled_messages WHERE id = $1")
            .bind(&rows[0].0)
            .fetch_one(&db.pool)
            .await
            .expect("interval");
    assert_eq!(interval, Some(90 * 60), "every-minutes becomes seconds");

    let content = mock.deferred_reply()["content"]
        .as_str()
        .expect("confirm text")
        .to_owned();
    assert!(
        content.contains(&format!("`{}` every 90m", rows[0].0)),
        "recurring confirmation renders the interval: {content}"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_create_rejects_invalid_input_and_audits_rejected() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    // No timing option: validation's missing-timing refusal, no row.
    runtime
        .on_interaction(&slash(
            "schedule",
            Some(CHANNEL),
            vec![option(
                "body",
                CommandOptionValue::String("hello".to_owned()),
            )],
        ))
        .await;
    // Out-of-range in-minutes: same refusal posture.
    runtime
        .on_interaction(&slash(
            "schedule",
            Some(CHANNEL),
            vec![
                option("body", CommandOptionValue::String("hello".to_owned())),
                option("in-minutes", CommandOptionValue::Integer(0)),
            ],
        ))
        .await;

    assert!(
        db.schedule_rows().await.is_empty(),
        "no rows on validation failures"
    );
    let edits: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH")
        .collect();
    assert_eq!(edits.len(), 2, "both refusals still complete the defer");
    let first: serde_json::Value = serde_json::from_slice(&edits[0].body).unwrap();
    assert_eq!(
        first["content"],
        "Give either in-minutes (one-shot) or every-minutes (recurring)."
    );
    let second: serde_json::Value = serde_json::from_slice(&edits[1].body).unwrap();
    assert_eq!(
        second["content"],
        "in-minutes must be between 1 and 525600, got 0."
    );
    let audits = db.schedule_audits().await;
    assert_eq!(audits.len(), 2, "both failures audit");
    assert!(
        audits
            .iter()
            .all(|(action, _, outcome)| action == "scheduled.create" && outcome == "rejected"),
        "validation failures audit scheduled.create/rejected: {audits:?}"
    );
    mock.shutdown().await;
    db.close().await;
}

// --- acceptance: quota-capacity refusal audits like other refusals --------
//
// Locked contract: one capacity refusal writes exactly one audit row with
// action `scheduled.create`, outcome `rejected` and reason `quota`, in the
// same shape as a validation rejection (fresh attempt id as `target_key`,
// invoking member as `actor_id`). A repeated refusal is a new refusal: it
// writes its own single row and no extra row (`1 refusal = 1 audit` parity).

/// (action, target_key, outcome, reason, actor_id) for this guild's rows in
/// the shared automation audit log.
async fn schedule_audit_rows(
    db: &TestDb,
) -> Vec<(
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
)> {
    sqlx::query_as(
        "SELECT action, target_key, outcome, reason, actor_id
           FROM automation_audit_log
          WHERE guild_id = $1 ORDER BY created_at, id",
    )
    .bind(GUILD_S)
    .fetch_all(&db.pool)
    .await
    .expect("automation audit rows with reasons")
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_create_quota_capacity_refusal_audits_one_rejected_row() {
    use two_bot_core::automation_quota::AutomationQuota;

    let db = TestDb::new().await;
    let limit = AutomationQuota::Schedules.limit();
    for n in 0..limit {
        db.seed_schedule(GUILD_S, &format!("schedule-{n}")).await;
    }
    let seeded: Vec<String> = db
        .schedule_rows()
        .await
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(seeded.len() as i64, limit, "guild starts at capacity");
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash(
            "schedule",
            Some(CHANNEL),
            vec![
                option("body", CommandOptionValue::String("hello".to_owned())),
                option("in-minutes", CommandOptionValue::Integer(30)),
            ],
        ))
        .await;

    assert_eq!(
        mock.deferred_reply()["content"],
        AutomationQuota::Schedules.to_string(),
        "capacity refusal keeps its existing reply text"
    );
    assert!(
        !mock
            .requests()
            .iter()
            .any(|request| request.path.starts_with("/api/v10/channels/")),
        "refusal posts nothing to a channel"
    );
    assert_eq!(
        db.schedule_rows()
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        seeded,
        "refusal writes no schedule row"
    );

    let audits = schedule_audit_rows(&db).await;
    assert_eq!(
        audits.len(),
        1,
        "one capacity refusal writes exactly one audit row: {audits:?}"
    );
    let (action, target_key, outcome, reason, actor_id) = &audits[0];
    assert_eq!(action, "scheduled.create");
    assert_eq!(outcome, "rejected");
    assert_eq!(reason.as_deref(), Some("quota"));
    assert_eq!(
        actor_id.as_deref(),
        Some("88"),
        "audit names the invoking member, like a validation rejection"
    );
    let target = target_key
        .as_deref()
        .expect("audit names the attempted schedule id");
    assert!(
        !target.is_empty() && !seeded.iter().any(|id| id == target),
        "target is the fresh attempt id, not a seeded row: {target}"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_create_refusals_audit_one_row_each_across_kinds() {
    use two_bot_core::automation_quota::AutomationQuota;

    let db = TestDb::new().await;
    for n in 0..AutomationQuota::Schedules.limit() {
        db.seed_schedule(GUILD_S, &format!("schedule-{n}")).await;
    }
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;
    let valid = || {
        slash(
            "schedule",
            Some(CHANNEL),
            vec![
                option("body", CommandOptionValue::String("hello".to_owned())),
                option("in-minutes", CommandOptionValue::Integer(30)),
            ],
        )
    };

    // Validation refusal (no timing), capacity refusal, the identical
    // capacity refusal again, then another validation refusal.
    runtime
        .on_interaction(&slash(
            "schedule",
            Some(CHANNEL),
            vec![option(
                "body",
                CommandOptionValue::String("hello".to_owned()),
            )],
        ))
        .await;
    runtime.on_interaction(&valid()).await;
    runtime.on_interaction(&valid()).await;
    runtime
        .on_interaction(&slash(
            "schedule",
            Some(CHANNEL),
            vec![
                option("body", CommandOptionValue::String("hello".to_owned())),
                option("in-minutes", CommandOptionValue::Integer(0)),
            ],
        ))
        .await;

    let edits: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH")
        .collect();
    assert_eq!(edits.len(), 4, "every refusal still completes the defer");
    let replies: Vec<String> = edits
        .iter()
        .map(|edit| {
            let json: serde_json::Value = serde_json::from_slice(&edit.body).unwrap();
            json["content"].as_str().expect("reply text").to_owned()
        })
        .collect();
    assert_eq!(
        replies,
        vec![
            "Give either in-minutes (one-shot) or every-minutes (recurring).".to_owned(),
            AutomationQuota::Schedules.to_string(),
            AutomationQuota::Schedules.to_string(),
            "in-minutes must be between 1 and 525600, got 0.".to_owned(),
        ],
        "refusal reply texts are unchanged"
    );
    assert_eq!(
        db.schedule_rows().await.len() as i64,
        AutomationQuota::Schedules.limit(),
        "no refusal writes a schedule row"
    );

    let audits = schedule_audit_rows(&db).await;
    assert_eq!(
        audits.len(),
        4,
        "each refusal writes exactly one audit row, no refusal writes an \
         extra row: {audits:?}"
    );
    assert!(
        audits.iter().all(|(action, _, outcome, _, actor_id)| {
            action == "scheduled.create"
                && outcome == "rejected"
                && actor_id.as_deref() == Some("88")
        }),
        "all four refusals share the rejected shape: {audits:?}"
    );
    let mut reasons: Vec<Option<String>> = audits
        .iter()
        .map(|(_, _, _, reason, _)| reason.clone())
        .collect();
    reasons.sort();
    assert_eq!(
        reasons,
        vec![
            Some("Give either in-minutes (one-shot) or every-minutes (recurring).".to_owned()),
            Some("in-minutes must be between 1 and 525600, got 0.".to_owned()),
            Some("quota".to_owned()),
            Some("quota".to_owned()),
        ],
        "validation reasons keep their error text; capacity reasons are quota"
    );
    let targets: std::collections::BTreeSet<Option<String>> = audits
        .iter()
        .map(|(_, target_key, _, _, _)| target_key.clone())
        .collect();
    assert_eq!(
        targets.len(),
        4,
        "each refusal audits its own fresh attempt id: {audits:?}"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn feed_quota_capacity_refusal_audit_behavior_is_unchanged() {
    use two_bot_core::automation_quota::AutomationQuota;

    let db = TestDb::new().await;
    for n in 0..AutomationQuota::Feeds.limit() {
        db.seed_feed(GUILD_S, &format!("feed-{n}"), "rss").await;
    }
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash(
            "feed-add",
            Some(CHANNEL),
            vec![
                option("kind", CommandOptionValue::String("rss".to_owned())),
                option(
                    "source",
                    CommandOptionValue::String("https://example.com/new.xml".to_owned()),
                ),
            ],
        ))
        .await;

    assert_eq!(
        mock.deferred_reply()["content"],
        AutomationQuota::Feeds.to_string()
    );
    assert_eq!(
        db.feed_rows().await.len() as i64,
        AutomationQuota::Feeds.limit(),
        "refusal writes no feed row"
    );
    assert!(
        db.feed_audits().await.is_empty(),
        "feed capacity refusal audit behavior is unchanged"
    );
    assert!(
        schedule_audit_rows(&db).await.is_empty(),
        "feed capacity refusal writes no automation audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_list_is_scoped_to_the_configured_guild_and_audits_nothing() {
    let db = TestDb::new().await;
    db.seed_schedule(GUILD_S, "sched-own").await;
    db.seed_schedule("9999", "sched-foreign").await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash("schedule-list", Some(CHANNEL), Vec::new()))
        .await;

    let json = mock.deferred_reply();
    let content = json["content"].as_str().expect("list content");
    assert!(content.contains("sched-own"), "own row listed: {content}");
    assert!(
        !content.contains("sched-foreign"),
        "other guild's row never listed: {content}"
    );
    assert!(
        db.schedule_audits().await.is_empty(),
        "list writes no audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

// --- schedule-list pagination: every configured id stays discoverable -----
//
// Locked contract for the builder (test-author handoff; do not edit these
// tests to fit an implementation):
//
// - `/schedule-list` gains ONE optional integer option, `page` (1-based,
//   minimum 1 in the published registry). Omitting it means page 1.
// - Each reply holds only whole rows in the existing row format
//   (`schedule_list_line`), ordered by (`next_run_at`, `id`), and never
//   exceeds 2,000 UTF-16 units. No id is ever split or silently dropped.
// - Whenever the guild's rows need more than one page, every page's reply
//   carries the indicator `Page <current> of <total>` (exact substring), so
//   staff can walk the pages and enumerate every configured id exactly once.
//   A single-page reply may or may not carry `Page 1 of 1`.
// - Empty guild: page 1 (or no page) replies `Nothing scheduled.`; any
//   higher page is out of range against a total of 1 page.
// - `page < 1` replies exactly `Page must be at least 1.`
// - `page > total` replies exactly
//   `Page <page> is out of range: this server has <total> page(s) of schedules.`
//   Refusals list no rows and must not overflow on extreme page values.
// - Listing stays guild-scoped, ephemeral, audit-free and write-free, and
//   keeps disabled/recurring markers visible on each row.

/// One full-quota fixture row: a realistic 32-hex id, a 19-digit channel
/// snowflake and a full ISO timestamp, so 25 rendered rows cannot fit in a
/// single 2,000-unit message (each row is at least ~84 units before state
/// markers).
struct ScheduleListFixtureRow {
    id: String,
    channel_id: String,
    next_run_at: String,
    interval_seconds: Option<i64>,
    enabled: bool,
}

/// The 25-row guild quota (`AutomationQuota::Schedules`), in the order the
/// list must return them: ascending (`next_run_at`, `id`). Rows 10 and 11
/// share a `next_run_at` so the `id` tie-break is load-bearing; every third
/// row recurs every 90 minutes; every fifth row is disabled.
fn schedule_list_fixture_rows() -> Vec<ScheduleListFixtureRow> {
    (0..25)
        .map(|n| {
            let minute = if n == 11 { 10 } else { n };
            ScheduleListFixtureRow {
                id: format!("a1b2c3d4e5f60718293a4b5c{n:08x}"),
                channel_id: format!("123456789012345{:04}", 6000 + n),
                next_run_at: format!("2030-01-01T00:{minute:02}:00.000Z"),
                interval_seconds: (n % 3 == 0).then_some(5400),
                enabled: n % 5 != 4,
            }
        })
        .collect()
}

/// Parse the `Page <current> of <total>` indicator out of a list reply.
fn schedule_list_page_indicator(content: &str) -> Option<(i64, i64)> {
    let rest = content.split("Page ").nth(1)?;
    let (current, rest) = rest.split_once(" of ")?;
    let total: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((current.trim().parse().ok()?, total.parse().ok()?))
}

/// The schedule rows in a list reply: lines in the row format, which start
/// with a backticked id. Page indicators and refusals are not rows.
fn schedule_list_row_lines(content: &str) -> Vec<&str> {
    content
        .lines()
        .filter(|line| line.starts_with('`'))
        .collect()
}

/// The complete id a row line leads with (between the first backticks).
fn schedule_list_row_id(line: &str) -> &str {
    line.trim_start_matches('`')
        .split('`')
        .next()
        .expect("row line leads with a backticked id")
}

/// Invoke `/schedule-list` through the shared runtime, optionally with the
/// `page` option.
async fn invoke_schedule_list(runtime: &Arc<CommandRuntime>, page: Option<i64>) {
    let options = page
        .map(|page| vec![option("page", CommandOptionValue::Integer(page))])
        .unwrap_or_default();
    runtime
        .on_interaction(&slash("schedule-list", Some(CHANNEL), options))
        .await;
}

/// Every deferred-reply completion body (`content`), in invocation order.
/// Also pins the transport shape shared by every page: one ephemeral defer
/// (type 5, flags 64) and one completion edit per invocation, nothing else.
fn schedule_list_reply_contents(mock: &MockRest, invocations: usize) -> Vec<String> {
    let requests = mock.requests();
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|r| r.method == "POST" && r.path.ends_with("/callback"))
        .collect();
    assert_eq!(
        callbacks.len(),
        invocations,
        "one ephemeral defer per list invocation"
    );
    for callback in &callbacks {
        let json: serde_json::Value =
            serde_json::from_slice(&callback.body).expect("callback json");
        assert_eq!(json["type"], 5, "deferred reply");
        assert_eq!(json["data"]["flags"], 64, "ephemeral defer");
    }
    let edits: Vec<_> = requests.iter().filter(|r| r.method == "PATCH").collect();
    assert_eq!(
        edits.len(),
        invocations,
        "one completion edit per list invocation, no extra messages"
    );
    assert_eq!(
        requests.len(),
        invocations * 2,
        "listing makes no REST calls beyond defer + completion"
    );
    edits
        .iter()
        .map(|edit| {
            let json: serde_json::Value =
                serde_json::from_slice(&edit.body).expect("completion json");
            json["content"]
                .as_str()
                .expect("list content is text")
                .to_owned()
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_list_full_quota_enumerates_every_id_across_pages() {
    let db = TestDb::new().await;
    let fixture = schedule_list_fixture_rows();
    assert_eq!(fixture.len(), 25, "the per-guild schedule quota");
    // Seed in reverse order: neither insertion order nor a bare
    // `next_run_at` sort may decide the tied pair's order.
    for row in fixture.iter().rev() {
        db.seed_schedule_fixture_row(GUILD_S, row).await;
    }
    db.seed_schedule("9999", "sched-foreign-full").await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    // Page 1 by default, then explicitly: the two must agree byte for byte.
    invoke_schedule_list(&runtime, None).await;
    invoke_schedule_list(&runtime, Some(1)).await;
    let first_two = schedule_list_reply_contents(&mock, 2);
    assert_eq!(
        first_two[0], first_two[1],
        "omitting page means page 1 exactly"
    );
    let first = &first_two[0];
    assert!(
        first.encode_utf16().count() <= 2000,
        "page 1 stays within Discord's 2,000 UTF-16 unit cap: {} units",
        first.encode_utf16().count()
    );
    let (current, total_pages) = schedule_list_page_indicator(first)
        .unwrap_or_else(|| panic!("a full-quota list must announce `Page 1 of <total>`: {first}"));
    assert_eq!(current, 1, "the first reply is page 1");
    assert!(
        (2..=25).contains(&total_pages),
        "25 whole rows cannot fit one capped message, so pages must continue: \
         indicator claims {total_pages} page(s)"
    );

    for page in 2..=total_pages {
        invoke_schedule_list(&runtime, Some(page)).await;
    }
    let contents = schedule_list_reply_contents(&mock, 1 + total_pages as usize);
    // contents[1] repeats page 1 (checked above); walk 1, 2, ..= total.
    let mut pages: Vec<&str> = vec![contents[0].as_str()];
    pages.extend(contents.iter().skip(2).map(String::as_str));

    let mut listed: Vec<&ScheduleListFixtureRow> = Vec::new();
    for (index, content) in pages.iter().enumerate() {
        let page = index as i64 + 1;
        assert!(
            content.encode_utf16().count() <= 2000,
            "page {page} stays within the 2,000 UTF-16 unit cap: {} units",
            content.encode_utf16().count()
        );
        assert_eq!(
            schedule_list_page_indicator(content),
            Some((page, total_pages)),
            "every page announces its position: {content}"
        );
        assert!(
            !content.contains("sched-foreign-full"),
            "another guild's schedule never leaks into page {page}: {content}"
        );
        let lines = schedule_list_row_lines(content);
        assert!(!lines.is_empty(), "page {page} carries at least one row");
        for line in lines {
            let id = schedule_list_row_id(line);
            let row = fixture
                .iter()
                .find(|row| row.id == id)
                .unwrap_or_else(|| panic!("row leads with a complete configured id: {line}"));
            assert!(
                line.contains(&format!("<#{}>", row.channel_id)),
                "row keeps its whole channel mention: {line}"
            );
            assert!(
                line.contains(&row.next_run_at),
                "row keeps its whole next-run timestamp: {line}"
            );
            if row.interval_seconds.is_some() {
                assert!(
                    line.contains(" every 90m"),
                    "recurring row keeps its interval marker: {line}"
                );
            } else {
                assert!(
                    !line.contains(" every "),
                    "one-shot row carries no interval marker: {line}"
                );
            }
            if row.enabled {
                assert!(
                    !line.contains("(disabled)"),
                    "enabled row carries no disabled marker: {line}"
                );
            } else {
                assert!(
                    line.contains(" (disabled)"),
                    "disabled row keeps its disabled marker: {line}"
                );
            }
            listed.push(row);
        }
    }

    let listed_ids: Vec<&str> = listed.iter().map(|row| row.id.as_str()).collect();
    let expected_ids: Vec<&str> = fixture.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        listed_ids, expected_ids,
        "walking every page enumerates each configured id exactly once, in \
         (next_run_at, id) order, with no tail silently dropped"
    );
    assert!(
        db.schedule_audits().await.is_empty(),
        "listing writes no audit rows on any page"
    );
    assert_eq!(
        db.schedule_rows().await.len(),
        25,
        "listing never writes schedule rows"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_list_empty_guild_pages_are_defined() {
    let db = TestDb::new().await;
    // Only another guild has schedules: this guild's list is empty on every
    // page, and the foreign id must never surface.
    db.seed_schedule("9999", "sched-foreign-only").await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    invoke_schedule_list(&runtime, None).await;
    invoke_schedule_list(&runtime, Some(1)).await;
    invoke_schedule_list(&runtime, Some(2)).await;

    let contents = schedule_list_reply_contents(&mock, 3);
    assert_eq!(contents[0], "Nothing scheduled.");
    assert_eq!(
        contents[1], "Nothing scheduled.",
        "explicit page 1 matches the default on an empty guild"
    );
    assert_eq!(
        contents[2], "Page 2 is out of range: this server has 1 page(s) of schedules.",
        "an empty guild has exactly one (empty) page"
    );
    for content in &contents {
        assert!(
            !content.contains("sched-foreign-only"),
            "foreign rows never surface: {content}"
        );
        assert!(content.encode_utf16().count() <= 2000);
    }
    assert!(db.schedule_audits().await.is_empty());
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_list_invalid_and_out_of_range_pages_refuse_without_writes() {
    let db = TestDb::new().await;
    let fixture = schedule_list_fixture_rows();
    for row in fixture.iter().take(3) {
        db.seed_schedule_fixture_row(GUILD_S, row).await;
    }
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    invoke_schedule_list(&runtime, Some(1)).await;
    invoke_schedule_list(&runtime, Some(0)).await;
    invoke_schedule_list(&runtime, Some(-1)).await;
    invoke_schedule_list(&runtime, Some(2)).await;
    invoke_schedule_list(&runtime, Some(i64::MAX)).await;

    let contents = schedule_list_reply_contents(&mock, 5);
    let single_page = &contents[0];
    assert!(single_page.encode_utf16().count() <= 2000);
    assert!(
        matches!(
            schedule_list_page_indicator(single_page),
            None | Some((1, 1))
        ),
        "a single-page list announces at most `Page 1 of 1`: {single_page}"
    );
    let single_ids: Vec<&str> = schedule_list_row_lines(single_page)
        .iter()
        .map(|line| schedule_list_row_id(line))
        .collect();
    assert_eq!(
        single_ids,
        fixture
            .iter()
            .take(3)
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        "page 1 lists the whole small guild in order"
    );
    assert_eq!(contents[1], "Page must be at least 1.");
    assert_eq!(contents[2], "Page must be at least 1.");
    assert_eq!(
        contents[3],
        "Page 2 is out of range: this server has 1 page(s) of schedules."
    );
    assert_eq!(
        contents[4],
        format!(
            "Page {} is out of range: this server has 1 page(s) of schedules.",
            i64::MAX
        ),
        "extreme page values refuse cleanly instead of overflowing page math"
    );
    for refusal in &contents[1..] {
        for row in fixture.iter().take(3) {
            assert!(
                !refusal.contains(&row.id),
                "a refused page lists no rows: {refusal}"
            );
        }
    }
    assert!(
        db.schedule_audits().await.is_empty(),
        "refused pages write no audit rows"
    );
    assert_eq!(
        db.schedule_rows().await.len(),
        3,
        "refused pages never write schedule rows"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_list_over_cap_keeps_truncation_visible_across_packed_pages() {
    let db = TestDb::new().await;
    // 27 rows (direct seeds bypass quota admission), inserted newest-first.
    // The listing cap is pinned at 25: raising it must break this test.
    for n in (0..27u32).rev() {
        db.seed_schedule_row(
            GUILD_S,
            &format!("cap-{n:02}"),
            &format!("2030-01-{:02}T00:00:00.000Z", n + 1),
            None,
            true,
        )
        .await;
    }
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    // Localized due-time rows pack into two replies. Walk both pages of the
    // first capped window so every listed row and the overflow remain visible.
    invoke_schedule_list(&runtime, None).await;
    invoke_schedule_list(&runtime, Some(2)).await;

    let contents = schedule_list_reply_contents(&mock, 2);
    assert_eq!(schedule_list_page_indicator(&contents[0]), Some((1, 2)));
    assert_eq!(schedule_list_page_indicator(&contents[1]), Some((2, 2)));
    let listed_ids: Vec<&str> = contents
        .iter()
        .flat_map(|content| schedule_list_row_lines(content))
        .map(schedule_list_row_id)
        .collect();
    let expected_ids: Vec<String> = (0..25u32).map(|n| format!("cap-{n:02}")).collect();
    assert_eq!(
        listed_ids,
        expected_ids.iter().map(String::as_str).collect::<Vec<_>>(),
        "the first capped window lists its 25 earliest ids exactly once"
    );
    for line in contents
        .iter()
        .flat_map(|content| schedule_list_row_lines(content))
    {
        assert!(
            line.contains("<t:")
                && line.contains(":F>. The 15s ticker and delivery may be delayed."),
            "each whole row includes its localized due time and uncertainty: {line}"
        );
    }
    assert!(
        contents[1].contains("(truncated, 2 more)") && contents[1].contains("Continued on page 3."),
        "the last first-window page names both the cut and the reachable continuation: {}",
        contents[1]
    );
    assert!(
        !contents
            .iter()
            .any(|content| content.contains("cap-25") || content.contains("cap-26")),
        "rows past the cap are counted, not listed: {contents:?}"
    );
    assert!(
        db.schedule_audits().await.is_empty(),
        "list writes no audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_list_at_cap_enumerates_every_row_without_an_overflow_line() {
    let db = TestDb::new().await;
    for n in (0..25u32).rev() {
        db.seed_schedule_row(
            GUILD_S,
            &format!("full-{n:02}"),
            &format!("2030-01-{:02}T00:00:00.000Z", n + 1),
            None,
            true,
        )
        .await;
    }
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    invoke_schedule_list(&runtime, None).await;
    let first = schedule_list_reply_contents(&mock, 1);
    let (current, total_pages) = schedule_list_page_indicator(&first[0])
        .expect("localized cap-sized list spans announced pages");
    assert_eq!(current, 1);
    assert!(
        total_pages >= 2,
        "localized rows require more than one page"
    );
    for page in 2..=total_pages {
        invoke_schedule_list(&runtime, Some(page)).await;
    }

    let contents = schedule_list_reply_contents(&mock, total_pages as usize);
    let listed_ids: Vec<&str> = contents
        .iter()
        .flat_map(|content| schedule_list_row_lines(content))
        .map(schedule_list_row_id)
        .collect();
    let expected_ids: Vec<String> = (0..25u32).map(|n| format!("full-{n:02}")).collect();
    assert_eq!(
        listed_ids,
        expected_ids.iter().map(String::as_str).collect::<Vec<_>>(),
        "exactly at the cap still exposes every configured id in order"
    );
    for (index, content) in contents.iter().enumerate() {
        assert_eq!(
            schedule_list_page_indicator(content),
            Some((index as i64 + 1, total_pages)),
            "every packed page announces its position"
        );
        assert!(
            !content.contains("truncated"),
            "no overflow line at the cap: {content}"
        );
        assert!(
            content.encode_utf16().count() <= 2000,
            "whole-row page stays within Discord's cap: {content}"
        );
    }
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_list_over_cap_third_page_lists_hidden_rows() {
    let db = TestDb::new().await;
    // The localized first window packs across pages 1 and 2, so page 3 must
    // reach the bounded hidden window instead of re-paging the first 25 rows.
    for n in (0..27u32).rev() {
        db.seed_schedule_row(
            GUILD_S,
            &format!("cap-{n:02}"),
            &format!("2030-01-{:02}T00:00:00.000Z", n + 1),
            None,
            true,
        )
        .await;
    }
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    invoke_schedule_list(&runtime, Some(3)).await;

    let contents = schedule_list_reply_contents(&mock, 1);
    let rows = schedule_list_row_lines(&contents[0]);
    assert_eq!(
        rows.iter()
            .map(|line| schedule_list_row_id(line))
            .collect::<Vec<_>>(),
        vec!["cap-25", "cap-26"],
        "page 3 lists exactly the rows past the capped first window"
    );
    for (line, due) in rows
        .iter()
        .zip(["2030-01-26T00:00:00.000Z", "2030-01-27T00:00:00.000Z"])
    {
        assert!(
            line.contains(due),
            "hidden row keeps its canonical due time: {line}"
        );
        assert!(
            line.contains("<t:")
                && line.contains(":F>. The 15s ticker and delivery may be delayed."),
            "hidden row retains its localized due time and uncertainty: {line}"
        );
    }
    assert!(
        db.schedule_audits().await.is_empty(),
        "list writes no audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_list_far_over_cap_walks_hidden_rows_and_names_remaining_overflow() {
    let db = TestDb::new().await;
    // 52 rows: the hidden window itself fills (25 rows) with 2 more behind
    // it. Localized rows may use several replies, but every hidden id remains
    // reachable and the terminal hidden page must name the remaining cut.
    for n in (0..52u32).rev() {
        db.seed_schedule_row(
            GUILD_S,
            &format!("big-{n:02}"),
            &format!("2030-03-01T{:02}:{:02}:00.000Z", n / 60, n % 60),
            None,
            true,
        )
        .await;
    }
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    invoke_schedule_list(&runtime, None).await;
    let first = schedule_list_reply_contents(&mock, 1);
    let (_, first_total) = schedule_list_page_indicator(&first[0])
        .expect("the localized first window announces its packed page count");
    assert!(
        first_total > 1,
        "localized rows require more than one first-window page: {}",
        first[0]
    );
    let first_hidden_page = first_total + 1;
    invoke_schedule_list(&runtime, Some(first_total)).await;
    let first_window = schedule_list_reply_contents(&mock, 2);
    assert!(
        first_window[1].contains(&format!("Continued on page {first_hidden_page}.")),
        "the terminal first-window page names the reachable hidden page: {}",
        first_window[1]
    );
    invoke_schedule_list(&runtime, Some(first_hidden_page)).await;
    let initial_contents = schedule_list_reply_contents(&mock, 3);
    let (_, total_pages) = schedule_list_page_indicator(&initial_contents[2])
        .expect("a multi-page hidden window carries global page numbers");
    assert_eq!(
        schedule_list_page_indicator(&initial_contents[2]).map(|(current, _)| current),
        Some(first_hidden_page),
        "the first page after the capped window starts the hidden range"
    );
    for page in first_hidden_page + 1..=total_pages {
        invoke_schedule_list(&runtime, Some(page)).await;
    }

    let invocations = 2 + usize::try_from(total_pages - first_total)
        .expect("global page total follows the first window");
    let contents = schedule_list_reply_contents(&mock, invocations);
    let hidden_ids: Vec<&str> = contents
        .iter()
        .skip(2)
        .flat_map(|content| schedule_list_row_lines(content))
        .map(schedule_list_row_id)
        .collect();
    let expected_hidden_ids: Vec<String> = (25..50u32).map(|n| format!("big-{n:02}")).collect();
    assert_eq!(
        hidden_ids,
        expected_hidden_ids
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        "walking every hidden page exposes the bounded second window in order"
    );
    let terminal_hidden_page = contents.last().expect("at least one hidden page");
    assert!(
        terminal_hidden_page.contains("(truncated, 2 more)"),
        "the terminal hidden page names rows beyond the bounded window: {terminal_hidden_page}"
    );
    assert!(
        !contents
            .iter()
            .any(|content| content.contains("big-50") || content.contains("big-51")),
        "rows past the hidden window are counted, not listed: {contents:?}"
    );
    for content in &contents {
        assert!(
            content.encode_utf16().count() <= 2000,
            "every packed response stays within Discord's cap: {content}"
        );
    }

    invoke_schedule_list(&runtime, Some(99)).await;
    let contents = schedule_list_reply_contents(&mock, invocations + 1);
    assert_eq!(
        contents.last(),
        Some(&format!(
            "Page 99 is out of range: this server has {total_pages} page(s) of schedules."
        )),
        "pages past both windows refuse with the combined page total"
    );
    assert!(
        db.schedule_audits().await.is_empty(),
        "list writes no audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_list_under_cap_preserves_rows_with_localized_due_and_uncertainty() {
    let db = TestDb::new().await;
    db.seed_schedule_row(
        GUILD_S,
        "weekly",
        "2030-02-03T04:05:06.000Z",
        Some(604_800),
        true,
    )
    .await;
    db.seed_schedule_row(GUILD_S, "once", "2030-01-01T00:00:00.000Z", None, true)
        .await;
    db.seed_schedule_row(
        GUILD_S,
        "paused",
        "2030-01-15T12:30:00.000Z",
        Some(3_600),
        false,
    )
    .await;
    db.seed_schedule_row(
        GUILD_S,
        "completed",
        "2026-10-01T00:00:00.000Z",
        None,
        false,
    )
    .await;
    let before = schedule_absolute_correction_tests::state(&db.pool).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash("schedule-list", Some(CHANNEL), Vec::new()))
        .await;

    // deferred_reply() already asserts the ephemeral defer (flags 64).
    let json = mock.deferred_reply();
    assert_eq!(
        json["content"],
        "`completed` `<#3333>` 2026-10-01T00:00:00.000Z (disabled)\n\
         `once` `<#3333>` 2030-01-01T00:00:00.000Z <t:1893456000:F>. The 15s ticker and delivery may be delayed.\n\
         `paused` `<#3333>` 2030-01-15T12:30:00.000Z every 60m (disabled)\n\
         `weekly` `<#3333>` 2030-02-03T04:05:06.000Z every 10080m <t:1896321906:F>. The 15s ticker and delivery may be delayed.",
        "list retains ordered complete rows, intervals and disabled state, with localized due times and delivery uncertainty only for enabled rows"
    );
    assert_eq!(
        schedule_absolute_correction_tests::state(&db.pool).await,
        before,
        "listing cannot change schedules, audits or commands"
    );
    assert!(
        db.schedule_audits().await.is_empty(),
        "list writes no audit row"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_remove_deletes_by_unique_prefix_and_refuses_ambiguous() {
    let db = TestDb::new().await;
    db.seed_schedule(GUILD_S, "abc111").await;
    db.seed_schedule(GUILD_S, "abc222").await;
    db.seed_schedule(GUILD_S, "xyz999").await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    // Unique prefix resolves and deletes one row.
    runtime
        .on_interaction(&slash(
            "schedule-remove",
            Some(CHANNEL),
            vec![option("id", CommandOptionValue::String("xyz".to_owned()))],
        ))
        .await;
    // Ambiguous prefix refuses and mutates nothing.
    runtime
        .on_interaction(&slash(
            "schedule-remove",
            Some(CHANNEL),
            vec![option("id", CommandOptionValue::String("abc".to_owned()))],
        ))
        .await;
    // No match refuses the same way.
    runtime
        .on_interaction(&slash(
            "schedule-remove",
            Some(CHANNEL),
            vec![option("id", CommandOptionValue::String("zzz".to_owned()))],
        ))
        .await;

    let remaining: Vec<String> = db
        .schedule_rows()
        .await
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(
        remaining,
        vec!["abc111".to_owned(), "abc222".to_owned()],
        "only the uniquely-resolved row is deleted"
    );
    let edits: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH")
        .collect();
    assert_eq!(edits.len(), 3, "one completion per remove");
    let removed: serde_json::Value = serde_json::from_slice(&edits[0].body).unwrap();
    assert_eq!(removed["content"], "Cancelled. Schedule `xyz999`.");
    let ambiguous: serde_json::Value = serde_json::from_slice(&edits[1].body).unwrap();
    assert_eq!(
        ambiguous["content"],
        "No unique scheduled message matches `abc`. Use the full id from /schedule-list."
    );
    let missing: serde_json::Value = serde_json::from_slice(&edits[2].body).unwrap();
    assert_eq!(
        missing["content"],
        "No unique scheduled message matches `zzz`. Use the full id from /schedule-list."
    );
    // Only the successful delete audits: ambiguous/missing prefixes resolve
    // to nothing and mutate nothing.
    assert_eq!(
        db.schedule_audits().await,
        vec![(
            "scheduled.delete".to_owned(),
            "xyz999".to_owned(),
            "ok".to_owned()
        )]
    );
    mock.shutdown().await;
    db.close().await;
}

/// `/schedule-remove` invalid ids refuse before any store access (TOG-29645,
/// IN-03 schedule-remove half): empty and over-128-UTF-16-unit ASCII and
/// multibyte ids through the real router against a never-usable lazy pool.
/// A store touch would fail the refused pool and reply with the generic store
/// failure; the documented refusal instead proves the handler guard returned
/// first. Only the permitted defer (POST /callback) + completion edit (PATCH)
/// are recorded — no Discord mutation beyond the acknowledgement.
#[tokio::test]
async fn schedule_remove_invalid_ids_refuse_before_database_access() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);

    // Lengths are UTF-16 units (the guard's unit), not bytes: `é` is one
    // unit, `😀` is two (a surrogate pair).
    let ascii_over = "x".repeat(129);
    let multibyte_over = "é".repeat(129);
    let surrogate_over = "😀".repeat(65);
    let hostile_over = format!("{}@everyone{}", "a".repeat(120), "b".repeat(20));
    assert_eq!(ascii_over.encode_utf16().count(), 129);
    assert_eq!(multibyte_over.encode_utf16().count(), 129);
    assert_eq!(surrogate_over.encode_utf16().count(), 65 * 2);
    assert!(hostile_over.encode_utf16().count() > 128);

    let empty_reply =
        "No unique scheduled message matches ``. Use the full id from /schedule-list.";
    let overlong_reply =
        "No unique scheduled message matches that id. Use the full id from /schedule-list.";
    let store_failure = "Schedule command failed; try again.";

    // Missing option and empty string both hit the empty guard.
    let cases: Vec<(Vec<CommandDataOption>, &str)> = vec![
        (Vec::new(), empty_reply),
        (
            vec![option("id", CommandOptionValue::String(String::new()))],
            empty_reply,
        ),
        (
            vec![option("id", CommandOptionValue::String(ascii_over))],
            overlong_reply,
        ),
        (
            vec![option("id", CommandOptionValue::String(multibyte_over))],
            overlong_reply,
        ),
        (
            vec![option("id", CommandOptionValue::String(surrogate_over))],
            overlong_reply,
        ),
        (
            vec![option("id", CommandOptionValue::String(hostile_over))],
            overlong_reply,
        ),
    ];
    let expected: Vec<String> = cases.iter().map(|(_, reply)| reply.to_string()).collect();
    for (options, _) in &cases {
        runtime
            .on_interaction(&slash("schedule-remove", Some(CHANNEL), options.clone()))
            .await;
    }

    let requests = mock.requests();
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 6, "one permitted defer per refusal");
    let edits: Vec<_> = requests.iter().filter(|r| r.method == "PATCH").collect();
    assert_eq!(edits.len(), 6, "one permitted completion edit per refusal");
    assert_eq!(
        requests.len(),
        12,
        "permitted defer+edit only, no other Discord call: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .all(|r| r.method == "POST" || r.method == "PATCH"),
        "no DELETE/PUT/message post on refusal: {requests:?}"
    );
    for (edit, want) in edits.iter().zip(expected.iter()) {
        let reply: serde_json::Value = serde_json::from_slice(&edit.body).unwrap();
        assert_eq!(&reply["content"], want);
        assert_ne!(
            &reply["content"], store_failure,
            "guard ran before any store access"
        );
        assert_eq!(reply["allowed_mentions"]["parse"], serde_json::json!([]));
    }
    // The overlong reply is a hard-coded literal: hostile input is never echoed.
    for edit in &edits[2..] {
        let reply: serde_json::Value = serde_json::from_slice(&edit.body).unwrap();
        let content = reply["content"].as_str().expect("string reply");
        assert_eq!(content, overlong_reply);
        assert!(
            !content.contains("@everyone"),
            "hostile tail must not echo: {content:?}"
        );
    }
    mock.shutdown().await;
}

/// `/schedule-remove` invalid ids mutate nothing durable (TOG-29645, IN-03
/// schedule-remove half): the same invalid matrix plus exact-128-unit boundary
/// and whitespace-only missing-ID controls through the real router against a
/// disposable database. Stored rows and audit rows are unchanged until the
/// valid-prefix control deletes exactly one row.
#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_remove_invalid_ids_leave_store_untouched() {
    let db = TestDb::new().await;
    db.seed_schedule(GUILD_S, "keep111").await;
    db.seed_schedule(GUILD_S, "keep222").await;
    db.seed_schedule(GUILD_S, "xyz999").await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    let ascii_over = "x".repeat(129);
    let multibyte_over = "é".repeat(129);
    let surrogate_over = "😀".repeat(65);
    let hostile_over = format!("{}@everyone{}", "a".repeat(120), "b".repeat(20));
    let ascii_exact = "x".repeat(128);
    let multibyte_exact = "é".repeat(128);
    assert_eq!(ascii_over.encode_utf16().count(), 129);
    assert_eq!(multibyte_over.encode_utf16().count(), 129);
    assert_eq!(surrogate_over.encode_utf16().count(), 65 * 2);
    assert_eq!(ascii_exact.encode_utf16().count(), 128);
    assert_eq!(multibyte_exact.encode_utf16().count(), 128);

    let empty_reply =
        "No unique scheduled message matches ``. Use the full id from /schedule-list.";
    let overlong_reply =
        "No unique scheduled message matches that id. Use the full id from /schedule-list.";
    let whitespace_reply =
        "No unique scheduled message matches `   `. Use the full id from /schedule-list.";
    let ascii_exact_reply = format!(
        "No unique scheduled message matches `{ascii_exact}`. Use the full id from /schedule-list."
    );
    let multibyte_exact_reply = format!(
        "No unique scheduled message matches `{multibyte_exact}`. Use the full id from /schedule-list."
    );

    // Invalid: missing, empty, overlong ASCII / multibyte / surrogate /
    // hostile. Boundary/missing controls: whitespace-only (a missing-ID
    // control under current behavior, not a pre-lookup refusal) and the two
    // exact-128-unit ids (they pass the guard, miss in the store, and echo).
    let invalid_cases: Vec<(Vec<CommandDataOption>, String)> = vec![
        (Vec::new(), empty_reply.to_owned()),
        (
            vec![option("id", CommandOptionValue::String(String::new()))],
            empty_reply.to_owned(),
        ),
        (
            vec![option("id", CommandOptionValue::String(ascii_over))],
            overlong_reply.to_owned(),
        ),
        (
            vec![option("id", CommandOptionValue::String(multibyte_over))],
            overlong_reply.to_owned(),
        ),
        (
            vec![option("id", CommandOptionValue::String(surrogate_over))],
            overlong_reply.to_owned(),
        ),
        (
            vec![option("id", CommandOptionValue::String(hostile_over))],
            overlong_reply.to_owned(),
        ),
        (
            vec![option("id", CommandOptionValue::String("   ".to_owned()))],
            whitespace_reply.to_owned(),
        ),
        (
            vec![option("id", CommandOptionValue::String(ascii_exact))],
            ascii_exact_reply,
        ),
        (
            vec![option("id", CommandOptionValue::String(multibyte_exact))],
            multibyte_exact_reply,
        ),
    ];
    for (options, _) in &invalid_cases {
        runtime
            .on_interaction(&slash("schedule-remove", Some(CHANNEL), options.clone()))
            .await;
    }

    // Forbidden effects: no row removed, no audit written.
    let remaining: Vec<String> = db
        .schedule_rows()
        .await
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(
        remaining,
        vec![
            "keep111".to_owned(),
            "keep222".to_owned(),
            "xyz999".to_owned()
        ],
        "invalid and boundary/missing ids delete nothing"
    );
    assert!(
        db.schedule_audits().await.is_empty(),
        "refusals write no scheduled.delete audit row"
    );

    // Permitted vs forbidden Discord effects: one defer + one completion edit
    // per refusal; no channel post, delete, or registry publish.
    let requests = mock.requests();
    assert_eq!(
        mock.posts_to("/callback").await.len(),
        9,
        "one permitted defer per refusal"
    );
    let edits: Vec<_> = requests.iter().filter(|r| r.method == "PATCH").collect();
    assert_eq!(edits.len(), 9, "one permitted completion edit per refusal");
    assert_eq!(
        requests.len(),
        18,
        "permitted defer+edit only on refusal: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .all(|r| r.method == "POST" || r.method == "PATCH"),
        "no DELETE/PUT/message post on refusal: {requests:?}"
    );
    assert!(mock.deletes().is_empty(), "no Discord delete on refusal");
    assert!(
        mock.posts_to("/messages").await.is_empty(),
        "no channel post on refusal"
    );
    for (edit, (_, want)) in edits.iter().zip(invalid_cases.iter()) {
        let reply: serde_json::Value = serde_json::from_slice(&edit.body).unwrap();
        assert_eq!(&reply["content"], want);
        assert_eq!(reply["allowed_mentions"]["parse"], serde_json::json!([]));
    }
    let hostile_edit: serde_json::Value =
        serde_json::from_slice(&edits[5].body).expect("hostile edit json");
    assert!(
        !hostile_edit["content"]
            .as_str()
            .expect("string reply")
            .contains("@everyone"),
        "overlong reply never echoes hostile input"
    );

    // Valid control: the uniquely-resolving prefix still deletes exactly one
    // row through the same router, proving the fixture was live and the
    // invalid inputs left the store usable.
    runtime
        .on_interaction(&slash(
            "schedule-remove",
            Some(CHANNEL),
            vec![option("id", CommandOptionValue::String("xyz".to_owned()))],
        ))
        .await;
    let remaining: Vec<String> = db
        .schedule_rows()
        .await
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(
        remaining,
        vec!["keep111".to_owned(), "keep222".to_owned()],
        "valid prefix still deletes its row after the invalid matrix"
    );
    assert_eq!(
        db.schedule_audits().await,
        vec![(
            "scheduled.delete".to_owned(),
            "xyz999".to_owned(),
            "ok".to_owned()
        )],
        "only the valid control audits"
    );
    let edits: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH")
        .collect();
    assert_eq!(edits.len(), 10, "one completion per interaction");
    let valid: serde_json::Value = serde_json::from_slice(&edits[9].body).unwrap();
    assert_eq!(valid["content"], "Cancelled. Schedule `xyz999`.");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_store_failure_replies_generic() {
    let db = TestDb::new().await;
    // Drop the table after migrations: every store call fails. No FK points
    // at scheduled_messages (0140/0141/0150), so no CASCADE is needed.
    sqlx::query("DROP TABLE scheduled_messages")
        .execute(&db.pool)
        .await
        .expect("drop scheduled_messages");
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash(
            "schedule",
            Some(CHANNEL),
            vec![
                option("body", CommandOptionValue::String("hello".to_owned())),
                option("in-minutes", CommandOptionValue::Integer(30)),
            ],
        ))
        .await;

    assert_eq!(
        mock.deferred_reply()["content"],
        "Schedule command failed; try again."
    );
    mock.shutdown().await;
    db.close().await;
}

// --- acceptance: store-failure refusals audit like other refusals ---------
//
// Locked contract: every schedule store-failure refusal (create read,
// create write, remove resolve, remove delete, list read) writes exactly
// one `automation_audit_log` row per refusal whenever the pool accepts it,
// in the existing refusal shape: the command's `scheduled.*` action,
// outcome `rejected`, the invoking member as `actor_id`, and a bounded
// single-token `reason` (a fixed error class such as the log line's
// `db_class` token, or an equivalent fixed token — never the raw sqlx /
// Postgres message, which can carry statement or fixture detail).
// `target_key` names the attempted row where one exists: the fresh attempt
// id for creates, the user-supplied prefix when resolution itself fails,
// the resolved id when the delete fails. A failed list has no schedule id,
// so its `target_key` carries none (NULL or empty) rather than inventing
// one. The user reply stays the existing fixed store-failure text. A dead
// pool (audit write itself impossible) degrades to that same safe reply
// with no panic and no leaked internals. Successful lists keep writing no
// audit row; only the refusal audits.

const SCHEDULE_STORE_FAILURE_REPLY: &str = "Schedule command failed; try again.";

/// Drop the schedule table inside this test's isolated schema: every store
/// call against it fails while `automation_audit_log` stays writable, so an
/// audit attempt on the live pool can still land.
async fn drop_scheduled_messages(db: &TestDb) {
    sqlx::query("DROP TABLE scheduled_messages")
        .execute(&db.pool)
        .await
        .expect("drop scheduled_messages");
}

/// Fail only INSERTs into `scheduled_messages`: the create path's read
/// still succeeds, so the refusal comes from the write itself.
async fn fail_scheduled_inserts(db: &TestDb) {
    sqlx::query(
        "CREATE OR REPLACE FUNCTION fail_scheduled_insert() RETURNS trigger AS $$
         BEGIN
           RAISE EXCEPTION 'induced scheduled insert failure';
         END;
         $$ LANGUAGE plpgsql",
    )
    .execute(&db.pool)
    .await
    .expect("install insert fault function");
    sqlx::query(
        "CREATE TRIGGER fail_scheduled_insert_trg
           BEFORE INSERT ON scheduled_messages FOR EACH ROW
           EXECUTE FUNCTION fail_scheduled_insert()",
    )
    .execute(&db.pool)
    .await
    .expect("install insert fault trigger");
}

/// Fail only DELETEs from `scheduled_messages`: prefix resolution still
/// succeeds, so the refusal comes from the delete itself.
async fn fail_scheduled_deletes(db: &TestDb) {
    sqlx::query(
        "CREATE OR REPLACE FUNCTION fail_scheduled_delete() RETURNS trigger AS $$
         BEGIN
           RAISE EXCEPTION 'induced scheduled delete failure';
         END;
         $$ LANGUAGE plpgsql",
    )
    .execute(&db.pool)
    .await
    .expect("install delete fault function");
    sqlx::query(
        "CREATE TRIGGER fail_scheduled_delete_trg
           BEFORE DELETE ON scheduled_messages FOR EACH ROW
           EXECUTE FUNCTION fail_scheduled_delete()",
    )
    .execute(&db.pool)
    .await
    .expect("install delete fault trigger");
}

/// The refusal shape every store-failure audit row must share.
fn assert_schedule_store_failure_audit(
    action: &str,
    outcome: &str,
    reason: Option<&str>,
    actor_id: Option<&str>,
    expected_action: &str,
) {
    assert_eq!(action, expected_action);
    assert_eq!(outcome, "rejected");
    assert_eq!(
        actor_id,
        Some("88"),
        "audit names the invoking member, like a validation rejection"
    );
    let reason = reason.expect("store-failure refusal audits a reason");
    assert!(
        !reason.is_empty() && reason.len() <= 64 && !reason.chars().any(char::is_whitespace),
        "reason is one bounded token, not raw error text: {reason:?}"
    );
    let lowered = reason.to_ascii_lowercase();
    for leaked in ["sqlx", "postgres", "scheduled_messages", "induced"] {
        assert!(
            !lowered.contains(leaked),
            "reason must not leak {leaked}: {reason:?}"
        );
    }
}

/// Every completion-edit `content`, in invocation order.
fn schedule_store_failure_replies(mock: &MockRest, invocations: usize) -> Vec<String> {
    let edits: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PATCH")
        .collect();
    assert_eq!(edits.len(), invocations, "one completion per invocation");
    edits
        .iter()
        .map(|edit| {
            let json: serde_json::Value =
                serde_json::from_slice(&edit.body).expect("completion json");
            json["content"]
                .as_str()
                .expect("reply content is text")
                .to_owned()
        })
        .collect()
}

fn schedule_create_interaction() -> Interaction {
    slash(
        "schedule",
        Some(CHANNEL),
        vec![
            option("body", CommandOptionValue::String("hello".to_owned())),
            option("in-minutes", CommandOptionValue::Integer(30)),
        ],
    )
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_create_read_store_failure_audits_one_rejected_row() {
    let db = TestDb::new().await;
    drop_scheduled_messages(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime.on_interaction(&schedule_create_interaction()).await;

    assert_eq!(
        mock.deferred_reply()["content"],
        SCHEDULE_STORE_FAILURE_REPLY
    );
    let audits = schedule_audit_rows(&db).await;
    assert_eq!(
        audits.len(),
        1,
        "one create read failure writes exactly one audit row: {audits:?}"
    );
    let (action, target_key, outcome, reason, actor_id) = &audits[0];
    assert_schedule_store_failure_audit(
        action,
        outcome,
        reason.as_deref(),
        actor_id.as_deref(),
        "scheduled.create",
    );
    let target = target_key
        .as_deref()
        .expect("audit names the attempted schedule id");
    assert!(!target.is_empty(), "target is the fresh attempt id");
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_create_put_store_failure_audits_one_rejected_row_per_refusal() {
    let db = TestDb::new().await;
    fail_scheduled_inserts(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime.on_interaction(&schedule_create_interaction()).await;
    runtime.on_interaction(&schedule_create_interaction()).await;

    assert_eq!(
        schedule_store_failure_replies(&mock, 2),
        vec![
            SCHEDULE_STORE_FAILURE_REPLY.to_owned(),
            SCHEDULE_STORE_FAILURE_REPLY.to_owned()
        ],
        "write failures keep the existing safe reply text"
    );
    assert!(
        db.schedule_rows().await.is_empty(),
        "failed writes store no schedule row"
    );
    let audits = schedule_audit_rows(&db).await;
    assert_eq!(
        audits.len(),
        2,
        "each write failure writes exactly one audit row: {audits:?}"
    );
    for (action, _, outcome, reason, actor_id) in &audits {
        assert_schedule_store_failure_audit(
            action,
            outcome,
            reason.as_deref(),
            actor_id.as_deref(),
            "scheduled.create",
        );
    }
    let targets: std::collections::BTreeSet<Option<String>> = audits
        .iter()
        .map(|(_, target_key, _, _, _)| target_key.clone())
        .collect();
    assert_eq!(
        targets.len(),
        2,
        "each refusal audits its own fresh attempt id: {audits:?}"
    );
    assert!(
        targets
            .iter()
            .all(|target| target.as_deref().is_some_and(|t| !t.is_empty())),
        "every target names an attempted schedule id: {audits:?}"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_remove_resolve_store_failure_audits_one_rejected_row() {
    let db = TestDb::new().await;
    drop_scheduled_messages(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash(
            "schedule-remove",
            Some(CHANNEL),
            vec![option(
                "id",
                CommandOptionValue::String("sched-xyz".to_owned()),
            )],
        ))
        .await;

    assert_eq!(
        mock.deferred_reply()["content"],
        SCHEDULE_STORE_FAILURE_REPLY
    );
    let audits = schedule_audit_rows(&db).await;
    assert_eq!(
        audits.len(),
        1,
        "one resolve failure writes exactly one audit row: {audits:?}"
    );
    let (action, target_key, outcome, reason, actor_id) = &audits[0];
    assert_schedule_store_failure_audit(
        action,
        outcome,
        reason.as_deref(),
        actor_id.as_deref(),
        "scheduled.delete",
    );
    assert_eq!(
        target_key.as_deref(),
        Some("sched-xyz"),
        "unresolved refusal names the user-supplied prefix"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_remove_delete_store_failure_audits_one_rejected_row() {
    let db = TestDb::new().await;
    db.seed_schedule(GUILD_S, "xyz999").await;
    fail_scheduled_deletes(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash(
            "schedule-remove",
            Some(CHANNEL),
            vec![option("id", CommandOptionValue::String("xyz".to_owned()))],
        ))
        .await;

    assert_eq!(
        mock.deferred_reply()["content"],
        SCHEDULE_STORE_FAILURE_REPLY
    );
    assert_eq!(
        db.schedule_rows()
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        vec!["xyz999".to_owned()],
        "failed delete leaves the schedule row in place"
    );
    let audits = schedule_audit_rows(&db).await;
    assert_eq!(
        audits.len(),
        1,
        "one delete failure writes exactly one audit row: {audits:?}"
    );
    let (action, target_key, outcome, reason, actor_id) = &audits[0];
    assert_schedule_store_failure_audit(
        action,
        outcome,
        reason.as_deref(),
        actor_id.as_deref(),
        "scheduled.delete",
    );
    assert_eq!(
        target_key.as_deref(),
        Some("xyz999"),
        "delete refusal names the resolved schedule id"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_list_store_failure_audits_one_rejected_row() {
    let db = TestDb::new().await;
    drop_scheduled_messages(&db).await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    runtime
        .on_interaction(&slash("schedule-list", Some(CHANNEL), Vec::new()))
        .await;

    assert_eq!(
        mock.deferred_reply()["content"],
        SCHEDULE_STORE_FAILURE_REPLY
    );
    let audits = schedule_audit_rows(&db).await;
    assert_eq!(
        audits.len(),
        1,
        "one list failure writes exactly one audit row: {audits:?}"
    );
    let (action, target_key, outcome, reason, actor_id) = &audits[0];
    assert_schedule_store_failure_audit(
        action,
        outcome,
        reason.as_deref(),
        actor_id.as_deref(),
        "scheduled.list",
    );
    assert!(
        target_key.as_deref().is_none_or(str::is_empty),
        "a failed list names no schedule id: {target_key:?}"
    );
    mock.shutdown().await;
    db.close().await;
}

// --- acceptance: deep-pagination list failures audit like other refusals --
//
// Locked contract: the two deep-pagination `/schedule-list` failure paths —
// a failed hidden-window read on a page past the first window's packed
// pages, and an unknown-overflow deep page whose count read failed — each
// write exactly one `automation_audit_log` row in the refusal shape above
// (`scheduled.list` / `rejected`, the invoking member as actor, no target,
// the failing read's bounded error-class token as reason), keeping the
// existing fixed store-failure reply. A successful deep page still writes
// no audit row and the page-1 over-cap render does not change.

/// Fail exactly the `scheduled_messages` SELECTs whose text matches
/// `pattern` (`%OFFSET%` selects only the hidden-window read, `%count(*)%`
/// only the overflow count; the capped first read matches neither). The
/// table is renamed under a same-named view whose `WHERE` raises on a
/// `current_query()` match, failing that one statement while the pool
/// stays live, so an audit write can still land. An RLS policy cannot do
/// this here: the test role is the database superuser, and superusers
/// bypass row security even under `FORCE`. Install only after seeding;
/// nothing writes `scheduled_messages` in these tests afterwards.
async fn fail_scheduled_selects_matching(db: &TestDb, pattern: &str) {
    sqlx::query(
        "CREATE OR REPLACE FUNCTION fail_query_matching(pat text) RETURNS boolean AS $$
         BEGIN
           IF current_query() LIKE pat THEN
             RAISE EXCEPTION 'induced scheduled select failure';
           END IF;
           RETURN TRUE;
         END;
         $$ LANGUAGE plpgsql VOLATILE",
    )
    .execute(&db.pool)
    .await
    .expect("install select fault function");
    assert!(
        !pattern.contains('\''),
        "fault pattern is a test-owned literal"
    );
    sqlx::query("ALTER TABLE scheduled_messages RENAME TO scheduled_messages_fault_base")
        .execute(&db.pool)
        .await
        .expect("rename scheduled_messages under the fault view");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE VIEW scheduled_messages AS
           SELECT * FROM scheduled_messages_fault_base
            WHERE fail_query_matching('{pattern}')"
    )))
    .execute(&db.pool)
    .await
    .expect("install select fault view");
}

/// The 27-row over-cap guild shared by the deep-pagination tests; direct
/// seeds bypass quota admission, and page 3 follows the two localized
/// first-window pages.
async fn seed_over_cap_schedule_rows(db: &TestDb) {
    for n in (0..27u32).rev() {
        db.seed_schedule_row(
            GUILD_S,
            &format!("cap-{n:02}"),
            &format!("2030-01-{:02}T00:00:00.000Z", n + 1),
            None,
            true,
        )
        .await;
    }
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_list_hidden_window_store_failure_audits_one_rejected_row() {
    let db = TestDb::new().await;
    seed_over_cap_schedule_rows(&db).await;
    fail_scheduled_selects_matching(&db, "%OFFSET%").await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    invoke_schedule_list(&runtime, Some(3)).await;

    assert_eq!(
        schedule_list_reply_contents(&mock, 1),
        vec![SCHEDULE_STORE_FAILURE_REPLY.to_owned()],
        "a failed hidden-window read keeps the existing safe reply text"
    );
    assert_eq!(
        db.schedule_rows().await.len(),
        27,
        "a failed deep page writes no schedule rows"
    );
    let audits = schedule_audit_rows(&db).await;
    assert_eq!(
        audits.len(),
        1,
        "one hidden-window failure writes exactly one audit row: {audits:?}"
    );
    let (action, target_key, outcome, reason, actor_id) = &audits[0];
    assert_schedule_store_failure_audit(
        action,
        outcome,
        reason.as_deref(),
        actor_id.as_deref(),
        "scheduled.list",
    );
    assert!(
        target_key.as_deref().is_none_or(str::is_empty),
        "a failed list names no schedule id: {target_key:?}"
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_list_unknown_overflow_store_failure_audits_one_rejected_row() {
    let db = TestDb::new().await;
    seed_over_cap_schedule_rows(&db).await;
    fail_scheduled_selects_matching(&db, "%count(*)%").await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    invoke_schedule_list(&runtime, Some(3)).await;

    assert_eq!(
        schedule_list_reply_contents(&mock, 1),
        vec![SCHEDULE_STORE_FAILURE_REPLY.to_owned()],
        "an unknown-overflow deep page keeps the existing safe reply text"
    );
    assert_eq!(
        db.schedule_rows().await.len(),
        27,
        "a failed deep page writes no schedule rows"
    );
    let audits = schedule_audit_rows(&db).await;
    assert_eq!(
        audits.len(),
        1,
        "one unknown-overflow refusal writes exactly one audit row: {audits:?}"
    );
    let (action, target_key, outcome, reason, actor_id) = &audits[0];
    assert_schedule_store_failure_audit(
        action,
        outcome,
        reason.as_deref(),
        actor_id.as_deref(),
        "scheduled.list",
    );
    assert!(
        target_key.as_deref().is_none_or(str::is_empty),
        "a failed list names no schedule id: {target_key:?}"
    );
    mock.shutdown().await;
    db.close().await;
}

/// Control: with the fault view installed but matching no statement, the
/// packed page-1 render and the successful hidden page retain their rows and
/// neither writes an audit row. This proves the fault harness itself, not just
/// the failures it induces, is inert.
#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_list_deep_pages_succeed_without_audit_when_no_query_fails() {
    let db = TestDb::new().await;
    seed_over_cap_schedule_rows(&db).await;
    fail_scheduled_selects_matching(&db, "%ZZZ_NO_SUCH_FRAGMENT%").await;
    let (runtime, mock) = db_runtime(&db, Vec::new()).await;

    invoke_schedule_list(&runtime, Some(1)).await;
    invoke_schedule_list(&runtime, Some(3)).await;

    let contents = schedule_list_reply_contents(&mock, 2);
    assert_eq!(
        schedule_list_page_indicator(&contents[0]),
        Some((1, 2)),
        "the first capped window remains packed across its localized pages"
    );
    let first_rows = schedule_list_row_lines(&contents[0]);
    assert_eq!(
        first_rows.first().map(|line| schedule_list_row_id(line)),
        Some("cap-00"),
        "page 1 starts at the earliest row"
    );
    assert!(
        first_rows.iter().all(|line| {
            line.contains("<t:")
                && line.contains(":F>. The 15s ticker and delivery may be delayed.")
        }),
        "the inert fault view preserves enriched page-1 rows: {}",
        contents[0]
    );
    let hidden_ids: Vec<&str> = schedule_list_row_lines(&contents[1])
        .iter()
        .map(|line| schedule_list_row_id(line))
        .collect();
    assert_eq!(
        hidden_ids,
        vec!["cap-25", "cap-26"],
        "a successful deep page still lists the hidden rows"
    );
    assert!(
        schedule_audit_rows(&db).await.is_empty(),
        "successful lists write no audit row on any page"
    );
    mock.shutdown().await;
    db.close().await;
}

/// Dead-pool control for a deep page: a closed pool fails the first read,
/// so no audit write is possible either; asking for page 3 must still
/// finish with the fixed safe reply — no panic, no leaked internals.
#[tokio::test]
async fn schedule_list_deep_page_on_dead_pool_replies_safely() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    pool.close().await;
    let runtime = CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(gates(true, true)),
        GUILD,
        true,
    );

    runtime
        .on_interaction(&slash(
            "schedule-list",
            Some(CHANNEL),
            vec![option("page", CommandOptionValue::Integer(3))],
        ))
        .await;

    let replies = schedule_store_failure_replies(&mock, 1);
    assert_eq!(
        replies,
        vec![SCHEDULE_STORE_FAILURE_REPLY.to_owned()],
        "a deep page on a dead pool degrades to the safe reply"
    );
    for leaked in ["sqlx", "postgres", "127.0.0.1", "agent_test", "Pool"] {
        assert!(
            !replies[0].contains(leaked),
            "dead-pool reply must not leak {leaked}: {}",
            replies[0]
        );
    }
    mock.shutdown().await;
}

/// Dead-pool control: with a closed pool, the audit write is impossible by
/// construction, so every schedule command must still finish with the
/// fixed safe reply — no panic, no hang, no sqlx internals, pool address
/// or credentials in the user text. A closed pool fails every query
/// immediately; an unreachable pool would hold each command for the full
/// acquire timeout, so this control stays fast after the audit attempt
/// adds a second store call per refusal.
#[tokio::test]
async fn schedule_store_failures_on_dead_pool_reply_safely() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    pool.close().await;
    let runtime = CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(gates(true, true)),
        GUILD,
        true,
    );

    runtime.on_interaction(&schedule_create_interaction()).await;
    runtime
        .on_interaction(&slash(
            "schedule-remove",
            Some(CHANNEL),
            vec![option(
                "id",
                CommandOptionValue::String("sched-xyz".to_owned()),
            )],
        ))
        .await;
    runtime
        .on_interaction(&slash("schedule-list", Some(CHANNEL), Vec::new()))
        .await;

    let replies = schedule_store_failure_replies(&mock, 3);
    assert_eq!(
        replies,
        vec![SCHEDULE_STORE_FAILURE_REPLY.to_owned(); 3],
        "dead pool degrades every schedule command to the safe reply"
    );
    for reply in &replies {
        for leaked in ["sqlx", "postgres", "127.0.0.1", "agent_test", "Pool"] {
            assert!(
                !reply.contains(leaked),
                "dead-pool reply must not leak {leaked}: {reply}"
            );
        }
    }
    mock.shutdown().await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn schedule_failed_defer_leaves_store_untouched() {
    let db = TestDb::new().await;
    let (runtime, mock) = db_runtime(&db, vec![404]).await;

    runtime
        .on_interaction(&slash(
            "schedule",
            Some(CHANNEL),
            vec![
                option("body", CommandOptionValue::String("hello".to_owned())),
                option("in-minutes", CommandOptionValue::Integer(30)),
            ],
        ))
        .await;

    assert!(
        db.schedule_rows().await.is_empty(),
        "no mutation when the defer fails"
    );
    assert!(db.schedule_audits().await.is_empty());
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

fn kick_as(permissions: Permissions, guild: u64) -> Interaction {
    let mut interaction = slash(
        "kick",
        Some(CHANNEL),
        vec![option("member", CommandOptionValue::User(Id::new(303)))],
    );
    interaction.member.as_mut().unwrap().permissions = Some(permissions);
    set_guild(&mut interaction, Some(Id::new(guild)));
    interaction
}

fn kick_gates(moderation: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
        moderation,
        ..gates(false, false)
    }
}

/// Wire a voice delegate that counts its calls and answers `claims`. The
/// stub never sends a callback itself, so a `true` answer must leave the
/// router silent and a `false` answer must leave exactly one router reply.
fn wire_kick_vote(runtime: &CommandRuntime, claims: bool) -> Arc<AtomicU64> {
    let calls = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&calls);
    runtime.set_voice_kick_vote(Arc::new(move |_| {
        observed.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move { claims })
    }));
    calls
}

async fn sole_ephemeral_reply(mock: &MockRest, case: &str) -> String {
    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 1, "{case} answers exactly once");
    let reply: serde_json::Value = serde_json::from_slice(&callbacks[0].body).unwrap();
    assert_eq!(reply["type"], 4, "{case}: immediate reply, not a defer");
    assert_eq!(reply["data"]["flags"], 64, "{case}: ephemeral reply");
    assert_eq!(mock.requests().len(), 1, "{case}: no other REST effects");
    reply["data"]["content"].as_str().unwrap().to_owned()
}

// The router is the only answerer of `/kick`. A passing moderation gate or the
// guild fence decides the interaction before the voice delegate is consulted,
// so sitting in a voice room never shields a member from a moderator and a
// foreign guild never reaches the vote path.
#[tokio::test]
async fn kick_moderation_outcomes_never_consult_the_voice_delegate() {
    let cases = [
        (
            "moderator",
            Permissions::all(),
            true,
            GUILD,
            "This command is not available in this build yet.".to_owned(),
        ),
        (
            "foreign guild",
            Permissions::all(),
            true,
            9999,
            RouterRefusal::GuildRestricted.message(),
        ),
    ];
    for (case, permissions, moderation, guild, expected) in cases {
        let (mock, origin) = MockRest::start(Vec::new()).await;
        let runtime = runtime_without_db(kick_gates(moderation), false, origin);
        // A delegate that would claim every interaction must still not run.
        let calls = wire_kick_vote(&runtime, true);
        runtime.on_interaction(&kick_as(permissions, guild)).await;
        assert_eq!(calls.load(Ordering::Relaxed), 0, "{case} skips voice");
        assert_eq!(sole_ephemeral_reply(&mock, case).await, expected, "{case}");
        mock.shutdown().await;
    }
}

// Moderation off, or the invoker lacking Kick Members, still serves the V4
// vote for room occupants: the delegate is asked once and, when it answers
// the interaction, the router stays silent.
#[tokio::test]
async fn refused_kick_hands_the_interaction_to_the_voice_delegate() {
    let cases = [
        ("missing permission", Permissions::empty(), true),
        ("moderation disabled", Permissions::all(), false),
    ];
    for (case, permissions, moderation) in cases {
        let (mock, origin) = MockRest::start(Vec::new()).await;
        let runtime = runtime_without_db(kick_gates(moderation), false, origin);
        let calls = wire_kick_vote(&runtime, true);
        runtime.on_interaction(&kick_as(permissions, GUILD)).await;
        assert_eq!(calls.load(Ordering::Relaxed), 1, "{case} asks voice once");
        assert!(
            mock.posts_to("/callback").await.is_empty(),
            "{case}: voice owns the callback, the router sends none"
        );
        assert!(mock.requests().is_empty(), "{case}: no other REST effects");
        mock.shutdown().await;
    }
}

// Voice declines (the invoker is not in the target's room, the target left
// between reads, or voice is off): the router refusal is the single reply.
// The same holds with no delegate wired at all.
#[tokio::test]
async fn registered_kick_with_a_mismatched_name_keeps_moderation_first_dispatch() {
    for payload_name in ["ping", "rsvp"] {
        for moderator in [true, false] {
            let (mock, origin) = MockRest::start(Vec::new()).await;
            let mut gates = kick_gates(true);
            gates.voice = true;
            let runtime = runtime_without_db(gates, false, origin);
            let mut registered =
                two_bot_discord::publish_commands(&[two_bot_core::CommandDefinition::new(
                    "kick",
                    "Synthetic kick command.",
                )]);
            registered[0].id = Some(Id::new(9001));
            runtime
                .executor()
                .command_identities()
                .replace_guild(1111, GUILD, &registered)
                .unwrap();
            let calls = wire_kick_vote(&runtime, true);
            let permissions = if moderator {
                Permissions::all()
            } else {
                Permissions::empty()
            };
            let mut interaction = kick_as(permissions, GUILD);
            let Some(InteractionData::ApplicationCommand(command)) = interaction.data.as_mut()
            else {
                unreachable!("slash fixture");
            };
            command.id = Id::new(9001);
            command.guild_id = Some(Id::new(GUILD));
            command.name = payload_name.into();
            runtime.dispatch_remaining(&Event::InteractionCreate(Box::new(InteractionCreate(
                interaction,
            ))));
            wait_for(
                || {
                    if moderator {
                        !mock.requests().is_empty()
                    } else {
                        calls.load(Ordering::Relaxed) == 1
                    }
                },
                "identity-resolved kick dispatch",
            )
            .await;
            let _dispatch_guard = runtime.dispatch_guard();
            if moderator {
                assert_eq!(calls.load(Ordering::Relaxed), 0, "moderation owns kick");
                assert_eq!(
                    sole_ephemeral_reply(&mock, payload_name).await,
                    "This command is not available in this build yet."
                );
            } else {
                assert_eq!(calls.load(Ordering::Relaxed), 1, "voice vote asked once");
                assert!(
                    mock.requests().is_empty(),
                    "the router sends no competing callback"
                );
            }
            mock.shutdown().await;
        }
    }
}

#[tokio::test]
async fn declined_kick_gets_the_router_refusal_exactly_once() {
    let cases = [
        (
            "missing permission",
            Permissions::empty(),
            true,
            RouterRefusal::ModerationPermission(two_bot_core::ModerationAction::Kick).message(),
        ),
        (
            "moderation disabled",
            Permissions::all(),
            false,
            RouterRefusal::ModerationDisabled.message(),
        ),
    ];
    for (case, permissions, moderation, expected) in cases {
        for wired in [true, false] {
            let case = format!("{case} (delegate wired: {wired})");
            let (mock, origin) = MockRest::start(Vec::new()).await;
            let runtime = runtime_without_db(kick_gates(moderation), false, origin);
            let calls = wired.then(|| wire_kick_vote(&runtime, false));
            runtime.on_interaction(&kick_as(permissions, GUILD)).await;
            if let Some(calls) = calls {
                assert_eq!(calls.load(Ordering::Relaxed), 1, "{case} asks voice once");
            }
            assert_eq!(sole_ephemeral_reply(&mock, &case).await, expected, "{case}");
            mock.shutdown().await;
        }
    }
}

#[tokio::test]
async fn self_role_reactions_have_their_own_bounded_lane() {
    use std::collections::HashSet;
    use twilight_model::gateway::payload::incoming::{MessageCreate, ReactionAdd};
    use two_bot_core::self_roles::{PanelMode, SelfRoleGates, SelfRoleOption, SelfRolePanel};
    use two_bot_core::InteractionRouter;
    use two_bot_cutover::self_role_store::SelfRoleStore;

    use crate::command_runtime::{DISPATCH_LIMITS, LANE_MESSAGES, LANE_REACTIONS};
    use crate::self_role_handlers::SelfRoleService;
    use crate::self_role_runtime::SelfRoleRuntime;

    const SR_GUILD: u64 = 100_000_000_000_000_001;
    const SR_CHANNEL: &str = "100000000000000007";
    const SR_MESSAGE: &str = "100000000000000008";
    const SR_BOT: &str = "100000000000000003";

    let (mock, origin) = MockRest::start(Vec::new()).await;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    let executor =
        ActionExecutor::with_proxy("test-token".to_owned(), Some(origin)).expect("mock executor");
    let service_runtime = SelfRoleRuntime {
        store: SelfRoleStore::new(pool.clone()),
        executor: executor.clone(),
        guild_id: SR_GUILD.to_string(),
        bot_id: SR_BOT.to_owned(),
    };
    let panel = SelfRolePanel {
        id: "games".into(),
        channel_id: SR_CHANNEL.into(),
        message_id: SR_MESSAGE.into(),
        mode: PanelMode::Reaction,
        exclusive: true,
        color: false,
        options: vec![SelfRoleOption {
            key: "new".into(),
            label: "New".into(),
            role_id: "100000000000000004".into(),
            permissions: "0".into(),
            emoji: Some("b".into()),
            description: None,
        }],
    };
    let service = Arc::new(
        SelfRoleService::new(
            service_runtime,
            SelfRoleGates {
                panels: vec![panel],
                dry_run: false,
            },
            &[SR_GUILD.to_string()].into_iter().collect::<HashSet<_>>(),
        )
        .expect("approved staging catalogue"),
    );
    let router = InteractionRouter::new(RouterGates {
        configured_guild: Some(SR_GUILD),
        scorecard: false,
        automations: true,
        announcements: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: true,
        onboarding_picker: false,
        session_picker: false,
    });
    let runtime =
        CommandRuntime::new_with_self_roles(pool, executor, router, SR_GUILD, true, service);
    let _guard = runtime.dispatch_guard();
    let reaction_cap = DISPATCH_LIMITS[LANE_REACTIONS];
    let message_cap = DISPATCH_LIMITS[LANE_MESSAGES];
    assert_ne!(
        LANE_REACTIONS, LANE_MESSAGES,
        "reactions must not share the message lane"
    );

    // A saturated message lane still admits reactions: message bursts must
    // not starve role claims.
    for _ in 0..message_cap {
        assert!(runtime.hold_lane_for_test(LANE_MESSAGES, Duration::from_secs(5)));
    }
    assert_eq!(runtime.lane_in_flight(LANE_MESSAGES), message_cap);
    let first = Event::ReactionAdd(Box::new(ReactionAdd(
        crate::self_role_handlers::tests::reaction(),
    )));
    assert!(
        runtime.dispatch(&first),
        "message saturation must not block reactions"
    );
    // Saturation held at dispatch time: the holders are fresh here, so a
    // slow reaction cannot drain them first and flake this read.
    assert_eq!(runtime.lane_in_flight(LANE_MESSAGES), message_cap);
    wait_for_timeout(
        || runtime.lane_in_flight(LANE_REACTIONS) == 0,
        "admitted reaction settles",
        Duration::from_secs(60),
    )
    .await;
    // Reaction work stays off the message lane: holders only drain, the
    // depth never grows past the cap no matter how slow the reaction was.
    assert!(
        runtime.lane_in_flight(LANE_MESSAGES) <= message_cap,
        "reaction work stays off the message lane"
    );
    wait_for_timeout(
        || runtime.lane_in_flight(LANE_MESSAGES) == 0,
        "message holders drain",
        Duration::from_secs(60),
    )
    .await;

    // One reaction is admitted while the lane has room; the detached task
    // settles (fast store refusal) and the lane drains.
    assert!(runtime.dispatch(&first), "room in the lane admits");
    wait_for_timeout(
        || runtime.lane_in_flight(LANE_REACTIONS) == 0,
        "admitted reaction settles",
        Duration::from_secs(60),
    )
    .await;

    // Saturate the dedicated reaction lane without database or REST work.
    for _ in 0..reaction_cap {
        assert!(runtime.hold_lane_for_test(LANE_REACTIONS, Duration::from_secs(30)));
    }
    assert_eq!(runtime.lane_in_flight(LANE_REACTIONS), reaction_cap);

    // A reaction burst must not discard message automations: with the
    // reaction lane full, a message still admits on its own lane without
    // touching the reaction depth.
    let message = Event::MessageCreate(Box::new(MessageCreate(message(
        9,
        100_000_000_000_000_007,
        false,
        Some(SR_GUILD),
    ))));
    assert!(
        runtime.dispatch(&message),
        "full reaction lane still admits messages"
    );
    // Admission is the isolation proof: `dispatch` returns true only after
    // pushing the message onto `LANE_MESSAGES`, so a saturated reaction lane
    // cannot discard message automations. The message's own sticky lookup is
    // incidental refused-database work; waiting on its settle would couple
    // this test to CI scheduler speed (the red-run failure), so assert the
    // synchronous separation facts instead: the message stays within its own
    // lane's cap and the reaction depth is untouched whether or not the
    // message task has been scheduled yet. The admitted task can only fail
    // its lookup and return; it never touches the mock REST double.
    assert!(
        runtime.lane_in_flight(LANE_MESSAGES) <= message_cap,
        "message work stays on the message lane"
    );
    assert_eq!(
        runtime.lane_in_flight(LANE_REACTIONS),
        reaction_cap,
        "message work stays off the reaction lane"
    );

    // A 1,000-event burst is dropped at the lane, never unbounded: every
    // dispatch reports refusal and in-flight never exceeds the cap.
    let mut admitted = 0usize;
    let mut max_observed = 0usize;
    for _ in 0..1000 {
        let event = Event::ReactionAdd(Box::new(ReactionAdd(
            crate::self_role_handlers::tests::reaction(),
        )));
        if runtime.dispatch(&event) {
            admitted += 1;
        }
        max_observed = max_observed.max(runtime.lane_in_flight(LANE_REACTIONS));
    }
    assert_eq!(admitted, 0, "saturated lane drops reaction bursts");
    assert!(
        max_observed <= reaction_cap,
        "in-flight {max_observed} exceeds lane cap {reaction_cap}"
    );
    assert_eq!(runtime.lane_in_flight(LANE_REACTIONS), reaction_cap);
    assert!(mock.requests().is_empty(), "dropped reactions send no REST");
    mock.shutdown().await;
}

#[tokio::test]
async fn self_role_reactions_enforce_per_member_fairness() {
    use std::collections::HashSet;
    use twilight_model::gateway::payload::incoming::{ReactionAdd, ReactionRemove};
    use twilight_model::id::Id;
    use two_bot_core::self_roles::{PanelMode, SelfRoleGates, SelfRoleOption, SelfRolePanel};
    use two_bot_core::InteractionRouter;
    use two_bot_cutover::self_role_store::SelfRoleStore;

    use crate::command_runtime::{DISPATCH_LIMITS, LANE_REACTIONS};
    use crate::interaction_admission::PER_USER_IN_FLIGHT;
    use crate::self_role_handlers::SelfRoleService;
    use crate::self_role_runtime::SelfRoleRuntime;

    const SR_GUILD: u64 = 100_000_000_000_000_001;
    const SR_CHANNEL: &str = "100000000000000007";
    const SR_MESSAGE: &str = "100000000000000008";
    const SR_BOT: &str = "100000000000000003";
    const MEMBER_A: u64 = 100_000_000_000_000_002;
    const MEMBER_B: u64 = 100_000_000_000_000_009;

    // Closure, not a nested fn item, so the outer `use` imports stay in scope.
    let reaction_for = |user: u64| {
        let mut reaction = crate::self_role_handlers::tests::reaction();
        reaction.user_id = Id::new(user);
        reaction
    };
    // One fresh self-role runtime per phase; each phase holds its own mock,
    // pool (lazy, never connected) and dispatch guard so phases never share
    // lane depth or per-member counts.
    macro_rules! fairness_runtime {
        () => {{
            let (mock, origin) = MockRest::start(Vec::new()).await;
            let pool = PgPoolOptions::new()
                .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
                .expect("lazy pool");
            let executor = ActionExecutor::with_proxy("test-token".to_owned(), Some(origin))
                .expect("mock executor");
            let service_runtime = SelfRoleRuntime {
                store: SelfRoleStore::new(pool.clone()),
                executor: executor.clone(),
                guild_id: SR_GUILD.to_string(),
                bot_id: SR_BOT.to_owned(),
            };
            let panel = SelfRolePanel {
                id: "games".into(),
                channel_id: SR_CHANNEL.into(),
                message_id: SR_MESSAGE.into(),
                mode: PanelMode::Reaction,
                exclusive: true,
                color: false,
                options: vec![SelfRoleOption {
                    key: "new".into(),
                    label: "New".into(),
                    role_id: "100000000000000004".into(),
                    permissions: "0".into(),
                    emoji: Some("b".into()),
                    description: None,
                }],
            };
            let service = Arc::new(
                SelfRoleService::new(
                    service_runtime,
                    SelfRoleGates {
                        panels: vec![panel],
                        dry_run: false,
                    },
                    &[SR_GUILD.to_string()].into_iter().collect::<HashSet<_>>(),
                )
                .expect("approved staging catalogue"),
            );
            let router = InteractionRouter::new(RouterGates {
                configured_guild: Some(SR_GUILD),
                scorecard: false,
                automations: true,
                announcements: false,
                moderation: false,
                voice: false,
                voice_assistant: false,
                tickets: false,
                self_roles: true,
                onboarding_picker: false,
                session_picker: false,
            });
            let runtime = CommandRuntime::new_with_self_roles(
                pool, executor, router, SR_GUILD, true, service,
            );
            (mock, runtime)
        }};
    }

    let reaction_cap = DISPATCH_LIMITS[LANE_REACTIONS];
    assert!(
        PER_USER_IN_FLIGHT < reaction_cap,
        "a single member burst must not fill the {reaction_cap}-slot reaction lane"
    );

    // Phase 1: member A at its per-member cap drops, member B still admits.
    // Burst occupancy is pinned with synchronous slot holds, so no settle
    // wait can flake this on a loaded runner.
    {
        let (mock, runtime) = fairness_runtime!();
        let _guard = runtime.dispatch_guard();
        let held: Vec<_> = (0..PER_USER_IN_FLIGHT)
            .map(|_| {
                runtime
                    .acquire_user_slot_for_test(MEMBER_A)
                    .expect("burst holds a slot")
            })
            .collect();
        assert!(
            runtime.acquire_user_slot_for_test(MEMBER_A).is_none(),
            "burst holds the whole per-member budget"
        );

        let add_a = Event::ReactionAdd(Box::new(ReactionAdd(reaction_for(MEMBER_A))));
        assert!(
            !runtime.dispatch(&add_a),
            "member at cap drops its excess reaction"
        );
        let remove_a = Event::ReactionRemove(Box::new(ReactionRemove(reaction_for(MEMBER_A))));
        assert!(
            !runtime.dispatch(&remove_a),
            "removes share the same per-member budget"
        );
        assert_eq!(
            runtime.lane_in_flight(LANE_REACTIONS),
            0,
            "dropped reactions spawn nothing"
        );
        assert_eq!(
            runtime.dispatch_drops_total(LANE_REACTIONS),
            2,
            "fairness drops share the lane's drop counter"
        );

        let add_b = Event::ReactionAdd(Box::new(ReactionAdd(reaction_for(MEMBER_B))));
        assert!(
            runtime.dispatch(&add_b),
            "other member still admits while the lane is hot"
        );
        assert!(
            runtime.lane_in_flight(LANE_REACTIONS) <= reaction_cap,
            "admitted reaction stays within the lane bound"
        );
        assert!(
            mock.requests().is_empty(),
            "fairness drops send no REST before settle"
        );

        drop(held);
        assert!(
            runtime.dispatch(&add_a),
            "released per-member slots admit again"
        );
        assert_eq!(
            runtime.dispatch_drops_total(LANE_REACTIONS),
            2,
            "admissions never increment the drop counter"
        );
        assert!(
            runtime.lane_in_flight(LANE_REACTIONS) <= reaction_cap,
            "readmitted reaction stays within the lane bound"
        );
        mock.shutdown().await;
    }

    // Phase 2: with fairness active, a hot saturated lane still drops excess
    // through the same bounded path instead of spawning unbounded work.
    {
        let (mock, runtime) = fairness_runtime!();
        let _guard = runtime.dispatch_guard();
        for _ in 0..reaction_cap {
            assert!(runtime.hold_lane_for_test(LANE_REACTIONS, Duration::from_secs(30)));
        }
        assert_eq!(runtime.lane_in_flight(LANE_REACTIONS), reaction_cap);

        let mut admitted = 0usize;
        let mut max_observed = 0usize;
        for i in 0..200 {
            let user = if i % 2 == 0 { MEMBER_A } else { MEMBER_B };
            let event = Event::ReactionAdd(Box::new(ReactionAdd(reaction_for(user))));
            if runtime.dispatch(&event) {
                admitted += 1;
            }
            max_observed = max_observed.max(runtime.lane_in_flight(LANE_REACTIONS));
        }
        assert_eq!(admitted, 0, "saturated lane drops multi-member bursts");
        assert!(
            max_observed <= reaction_cap,
            "in-flight {max_observed} exceeds lane cap {reaction_cap}"
        );
        assert_eq!(runtime.lane_in_flight(LANE_REACTIONS), reaction_cap);
        assert!(mock.requests().is_empty(), "dropped reactions send no REST");
        mock.shutdown().await;
    }
}

/// Saturation drops count per lane while the log stays quiet (TOG-19878): a
/// burst of N drops on one lane increments only that lane's counter and
/// emits O(1) log lines. The busy lane is covered through the same
/// `spawn_first` path.
#[tokio::test]
async fn saturated_lane_counts_drops_per_lane_and_quiets_logs() {
    use crate::command_runtime::{DISPATCH_LIMITS, LANE_BUSY, LANE_MESSAGES};
    use crate::tracing_capture;

    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(false, false), false, origin);
    let _guard = runtime.dispatch_guard();

    // Fill the message lane without database or REST work.
    for _ in 0..DISPATCH_LIMITS[LANE_MESSAGES] {
        assert!(runtime.hold_lane_for_test(LANE_MESSAGES, Duration::from_secs(30)));
    }
    assert_eq!(
        runtime.lane_in_flight(LANE_MESSAGES),
        DISPATCH_LIMITS[LANE_MESSAGES]
    );

    const MESSAGE_DROPS: usize = 20;
    const BUSY_DROPS: usize = 3;
    let capture = tracing_capture::Capture::default();
    tracing::subscriber::with_default(capture.clone(), || {
        for _ in 0..MESSAGE_DROPS {
            assert!(
                !runtime.hold_lane_for_test(LANE_MESSAGES, Duration::from_secs(30)),
                "a full lane admits no waiters"
            );
        }
        // The busy lane drops through the same path with its own counter.
        for _ in 0..DISPATCH_LIMITS[LANE_BUSY] {
            assert!(runtime.hold_lane_for_test(LANE_BUSY, Duration::from_secs(30)));
        }
        for _ in 0..BUSY_DROPS {
            assert!(
                !runtime.hold_lane_for_test(LANE_BUSY, Duration::from_secs(30)),
                "a full busy lane admits no waiters"
            );
        }
    });

    assert_eq!(
        runtime.dispatch_drops_total(LANE_MESSAGES),
        MESSAGE_DROPS as u64,
        "every message-lane drop counts"
    );
    assert_eq!(
        runtime.dispatch_drops_total(LANE_BUSY),
        BUSY_DROPS as u64,
        "every busy-lane drop counts on its own lane"
    );
    for lane in 0..DISPATCH_LIMITS.len() {
        if lane != LANE_MESSAGES && lane != LANE_BUSY {
            assert_eq!(
                runtime.dispatch_drops_total(lane),
                0,
                "lane {lane} must not count another lane's drops"
            );
        }
    }
    let saturated_lines = capture
        .text()
        .lines()
        .filter(|line| line.contains("command dispatch saturated"))
        .count();
    assert_eq!(
        saturated_lines,
        1,
        "a burst of {} drops emits one sampled line, got {saturated_lines}",
        MESSAGE_DROPS + BUSY_DROPS
    );
    assert!(mock.requests().is_empty(), "dropped work sends no REST");
    mock.shutdown().await;
}

/// Same-path sticky witness: real worker verdicts, the worker trigger gate,
/// detached dispatch, and the sticky store/REST hook. Both creates match the
/// runtime and seeded row; only automod's scope changes, so guild rejection
/// cannot masquerade as a successful Bypassed fence.
#[tokio::test]
async fn worker_trigger_gate_observes_sticky_http_and_state() {
    use std::collections::HashMap;

    use twilight_model::gateway::payload::incoming::MessageCreate;
    use two_bot_core::automod_runtime::{
        AutomodRuntime, AutomodScope, FunnelDisposition, MessageSubject, TargetFacts,
        STAGING_GUILD_ID,
    };
    use two_bot_core::automod_store::AutomodStore;
    use two_bot_core::sticky::PutSticky;
    use two_bot_core::AutomodConfig;
    use two_bot_discord::automod::event_to_automod;
    use two_bot_discord::automod_activation::{AutomodActivation, AutomodFacts, FetchedMessage};

    use crate::automod_gateway::{process, WorkerVerdict};
    use crate::command_runtime::LANE_MESSAGES;
    use crate::gateway::worker_prefix_trigger;

    struct NoFetch;
    impl AutomodFacts for NoFetch {
        async fn fetch_message(&self, _: &str, _: &str, _: &str) -> Option<FetchedMessage> {
            panic!("complete create needs no REST enrichment");
        }
        async fn target_facts(&self, _: &MessageSubject) -> Option<TargetFacts> {
            panic!("clean create needs no enforcement facts");
        }
    }

    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP worker_trigger_gate_observes_sticky_http_and_state: TWO_TEST_DATABASE_URL is not set");
        return;
    };
    let db =
        two_bot_testsupport::TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("migrated disposable test database");
    let guild = STAGING_GUILD_ID.parse::<u64>().expect("fixture guild");
    assert!(store::put_sticky(
        db.pool(),
        &PutSticky {
            guild_id: STAGING_GUILD_ID,
            channel_id: CHANNEL_S,
            body: "remember this",
            debounce_seconds: 5,
            enabled: true,
            actor_id: "77",
            now_ms: now_millis_for_test(),
        },
    )
    .await
    .expect("seed due sticky"));
    let sticky_row = || async {
        sqlx::query_scalar::<_, sqlx::types::Json<serde_json::Value>>(
            "SELECT to_jsonb(s) FROM sticky_messages s WHERE guild_id = $1 AND channel_id = $2",
        )
        .bind(STAGING_GUILD_ID)
        .bind(CHANNEL_S)
        .fetch_one(db.pool())
        .await
        .expect("full sticky row, including claim and timestamps")
        .0
    };
    let audits = || async {
        sqlx::query_as::<_, (String, String, Option<String>)>(
            "SELECT action, outcome, target_key FROM automation_audit_log WHERE guild_id = $1 ORDER BY created_at, id",
        )
        .bind(STAGING_GUILD_ID)
        .fetch_all(db.pool())
        .await
        .expect("sticky audit rows")
    };
    let before = sticky_row().await;
    assert!(before["last_message_id"].is_null(), "fixture is due");
    assert!(audits().await.is_empty());

    let (mock, origin) = MockRest::start(Vec::new()).await;
    let executor = executor_at(origin);
    let mut router_gates = gates(true, false);
    router_gates.configured_guild = Some(guild);
    let runtime = CommandRuntime::new(
        db.pool().clone(),
        executor.clone(),
        router_with_commands(router_gates),
        guild,
        true,
    );
    let dispatch_guard = runtime.dispatch_guard();
    let vars = HashMap::from([("TWO_AUTOMOD".to_owned(), "1".to_owned())]);
    let config = AutomodConfig::from_map(&vars).expect("enabled automod");
    let activation = |scope_guild: &str| {
        AutomodActivation::new(
            AutomodRuntime::new(
                config.clone(),
                AutomodScope {
                    guild_id: scope_guild.to_owned(),
                    live_approved: false,
                },
            ),
            AutomodStore::new(db.pool().clone()),
            NoFetch,
            executor.clone(),
        )
    };
    let create = |id| {
        let mut msg = message(id, CHANNEL, false, Some(guild));
        msg.member = Some(
            serde_json::from_value(serde_json::json!({
                "roles": [], "joined_at": null, "deaf": false, "mute": false, "flags": 0
            }))
            .expect("complete member facts"),
        );
        Event::MessageCreate(Box::new(MessageCreate(msg)))
    };
    let dispatch = |event: &Event, verdict| {
        worker_prefix_trigger(Some(verdict), event, true)
            .is_some_and(|trigger| runtime.dispatch_with_verdict(event, trigger))
    };
    let bypassed_event = create(61);
    let bypassed = process(
        &activation("42"),
        event_to_automod(&bypassed_event, 1).expect("Bypassed delivery from the same event"),
        "2026-10-02T00:00:00.000Z",
    )
    .await;
    assert_eq!(
        bypassed,
        WorkerVerdict {
            funnel: FunnelDisposition::Accept,
            trigger: FunnelDisposition::CaptureOnly,
        }
    );
    let admitted = dispatch(&bypassed_event, bypassed);
    wait_for(
        || runtime.lane_in_flight(LANE_MESSAGES) == 0,
        "Bypassed worker work settles",
    )
    .await;
    assert!(mock.requests().is_empty(), "Bypassed posts no sticky HTTP");
    assert_eq!(
        sticky_row().await,
        before,
        "Bypassed mutates no sticky state"
    );
    assert!(audits().await.is_empty(), "Bypassed writes no sticky audit");
    assert!(!admitted, "Bypassed never admits message work");

    let accepted_event = create(62);
    let accepted = process(
        &activation(STAGING_GUILD_ID),
        event_to_automod(&accepted_event, 2).expect("Accept delivery from the same event"),
        "2026-10-02T00:00:00.000Z",
    )
    .await;
    assert_eq!(
        accepted,
        WorkerVerdict {
            funnel: FunnelDisposition::Accept,
            trigger: FunnelDisposition::Accept,
        }
    );
    assert!(
        dispatch(&accepted_event, accepted),
        "Accept admits sticky work"
    );
    wait_for(
        || runtime.lane_in_flight(LANE_MESSAGES) == 0,
        "accepted sticky POST, state update and audit settle",
    )
    .await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "exactly one sticky HTTP effect");
    assert_eq!(requests[0].method, "POST");
    assert_eq!(
        requests[0].path,
        format!("/api/v10/channels/{CHANNEL}/messages")
    );
    let posted: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(posted["content"], "remember this");
    assert_eq!(posted["enforce_nonce"], true);
    let after = sticky_row().await;
    assert_eq!(after["last_message_id"], "9000000000000000000");
    assert!(
        after["last_posted_at"].is_string(),
        "POST updates sticky state"
    );
    assert!(after["claim_token"].is_null(), "record releases claim");
    assert!(after["claimed_at"].is_null(), "record clears claim time");
    assert_eq!(
        after["delivery_uncertain"], false,
        "record disarms the delivery fence"
    );
    assert_eq!(
        audits().await,
        vec![(
            "sticky.run".to_owned(),
            "ok".to_owned(),
            Some(CHANNEL_S.to_owned())
        )]
    );
    drop(dispatch_guard);
    drop(runtime);
    mock.shutdown().await;
    db.close().await.expect("drop own disposable database");
}

/// Worker call-site proof for the verdict-to-prefix-trigger plumbing: with
/// automod enabled, driving `dispatch_with_verdict` with the worker decision
/// (`gateway::worker_prefix_trigger`, exactly as `apply_dispatch` wires it)
/// posts exactly one prefix reply for an `Accept` create and none for a
/// `CaptureOnly` or unscreened (`None`) create.
///
/// Seeded `!faq` row on the isolated `TestDb`, loopback `MockRest`, the real
/// trigger handler and the real render. A dispatch path that drops the
/// verdict (`None`), forces `Accept`, or ignores the verdict for the
/// configured acceptance posts zero or two-plus replies and fails the count;
/// a worker that forwards the funnel disposition instead of the trigger
/// verdict upgrades the capture-only create to a reply and fails it too.
/// Requires the disposable agent-testdb/CI Postgres service (same as the
/// gateway suite — never the runtime DATABASE_URL).
#[tokio::test]
#[ignore = "requires isolated agent-testdb or CI service"]
async fn worker_verdict_drives_prefix_trigger_from_call_site() {
    use std::collections::HashMap;

    use twilight_model::gateway::payload::incoming::MessageCreate;
    use two_bot_core::automod_runtime::FunnelDisposition;
    use two_bot_core::custom_commands::PutCommandInput;

    use crate::automod_gateway::WorkerVerdict;
    use crate::command_runtime::LANE_MESSAGES;
    use crate::gateway::worker_prefix_trigger;
    use crate::gateway_commands::GatewayCommandConfig;

    fn verdict_message(id: u64, author: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "id": id.to_string(), "guild_id": "2222", "channel_id": "4444", "type": 0,
            "author": {"id": author, "username": "tester", "discriminator": "0000", "avatar": null},
            "content": "!faq please", "timestamp": "2026-09-30T12:00:00.000000+00:00",
            "edited_timestamp": null,
            "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
            "attachments": [], "embeds": [], "pinned": false
        }))
        .expect("valid Twilight message fixture")
    }

    fn verdict_event(message: Message) -> Event {
        Event::MessageCreate(Box::new(MessageCreate(message)))
    }

    let db = TestDb::new().await;
    two_bot_core::custom_command_service::put(
        &db.pool,
        true,
        GUILD_S,
        "3333",
        &PutCommandInput {
            name: "faq".to_owned(),
            description: "FAQ".to_owned(),
            template: "Hi {user} {username} in {server} {channel}".to_owned(),
            text_trigger: Some("!faq".to_owned()),
        },
        "seed-faq",
        &two_bot_core::now_iso(),
    )
    .await
    .expect("seed custom command");

    // Bootstrap reads first: the application id, then the guild name.
    let (mock, origin) = MockRest::start_script(vec![
        RestResponse {
            status: 200,
            body: Some(r#"{"id":"1111"}"#.to_owned()),
            delay: Duration::ZERO,
        },
        RestResponse {
            status: 200,
            body: Some(r#"{"id":"2222","name":"Test guild"}"#.to_owned()),
            delay: Duration::ZERO,
        },
    ])
    .await;
    let runtime = CommandRuntime::new(
        db.pool.clone(),
        executor_at(origin),
        router_with_commands(gates(true, true)),
        GUILD,
        true,
    );
    let _guard = runtime.dispatch_guard();
    let vars = HashMap::from([
        ("TWO_AUTOMATIONS".to_owned(), "1".to_owned()),
        ("TWO_TEXT_COMMANDS".to_owned(), "1".to_owned()),
        ("TWO_AUTOMOD".to_owned(), "1".to_owned()),
    ]);
    runtime
        .initialize_custom_commands(
            GatewayCommandConfig::from_map_for_activation(GUILD, &vars, true)
                .expect("dispatch command config"),
        )
        .await
        .expect("custom-command bootstrap (loopback REST, isolated DB)");

    // The worker call site: the trigger verdict feeds `dispatch_with_verdict`.
    let accept_event = verdict_event(verdict_message(61, "3333"));
    let split_event = verdict_event(verdict_message(62, "3334"));
    let unscreened_event = verdict_event(verdict_message(63, "3335"));
    let accept = Some(WorkerVerdict {
        funnel: FunnelDisposition::Accept,
        trigger: FunnelDisposition::Accept,
    });
    let split = Some(WorkerVerdict {
        funnel: FunnelDisposition::Accept,
        trigger: FunnelDisposition::CaptureOnly,
    });
    let accept_trigger = worker_prefix_trigger(accept, &accept_event, true);
    assert_eq!(
        accept_trigger,
        Some(Some(FunnelDisposition::Accept)),
        "accepted create forwards its accept trigger"
    );
    // Fail-closed: the split create never dispatches, so it can post no
    // reply and enqueue no sticky work; the `posts.len() == 1` assertion
    // below proves it.
    let split_trigger = worker_prefix_trigger(split, &split_event, true);
    assert!(
        split_trigger.is_none(),
        "uninspected funnel-accept never dispatches prefix triggers"
    );
    let unscreened_trigger = worker_prefix_trigger(None, &unscreened_event, true);
    assert_eq!(
        unscreened_trigger,
        Some(None),
        "unscreened create dispatches without a verdict and fails closed downstream"
    );
    assert!(
        runtime.dispatch_with_verdict(&accept_event, accept_trigger.expect("accept dispatches")),
        "accept admits on the message lane"
    );
    assert!(
        runtime.dispatch_with_verdict(
            &unscreened_event,
            unscreened_trigger.expect("unscreened dispatches")
        ),
        "unscreened create still admits; the missing verdict refuses it"
    );
    wait_for(
        || runtime.lane_in_flight(LANE_MESSAGES) == 0,
        "verdict prefix work settles",
    )
    .await;

    let posts = mock.posts_to("/channels/4444/messages").await;
    assert_eq!(
        posts.len(),
        1,
        "only the accepted create posts a prefix reply, got {}",
        posts.len()
    );
    let body: serde_json::Value = serde_json::from_slice(&posts[0].body).unwrap();
    assert_eq!(
        body["content"], "Hi <@3333> tester in Test guild <#4444>",
        "accepted create renders and posts the seeded trigger reply"
    );
    mock.shutdown().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// `/privacy` member self-serve (parity §1 row 32)
// ---------------------------------------------------------------------------

fn privacy_interaction(member: u64, subcommand: Option<&str>) -> Interaction {
    let options = subcommand
        .map(|name| vec![option(name, CommandOptionValue::SubCommand(Vec::new()))])
        .unwrap_or_default();
    let base = slash("privacy", Some(CHANNEL), options);
    from_member(base, member, Permissions::empty())
}

/// Guild-scoped privacy interaction for store-backed tests: the privacy
/// service validates guild and member as Discord snowflakes (17–20 digits),
/// so the small `GUILD`/`88` fixtures used elsewhere would refuse before any
/// purge. Callers pass real-shaped snowflake IDs for both.
fn privacy_interaction_in(guild: u64, member: u64, subcommand: Option<&str>) -> Interaction {
    let mut interaction = privacy_interaction(member, subcommand);
    set_guild(&mut interaction, Some(Id::new(guild)));
    interaction
}

fn privacy_request_interaction_in(
    guild: u64,
    member: u64,
    subcommand: &str,
    request_id: Option<&str>,
) -> Interaction {
    let nested = request_id
        .map(|request_id| {
            vec![option(
                "request-id",
                CommandOptionValue::String(request_id.to_owned()),
            )]
        })
        .unwrap_or_default();
    let mut interaction = from_member(
        slash(
            "privacy",
            Some(CHANNEL),
            vec![option(subcommand, CommandOptionValue::SubCommand(nested))],
        ),
        member,
        Permissions::empty(),
    );
    set_guild(&mut interaction, Some(Id::new(guild)));
    interaction
}

#[test]
fn privacy_action_decodes_known_analytics_and_deletion_subcommands() {
    assert_eq!(
        privacy_action(&privacy_interaction(88, Some("opt-out"))),
        PrivacyAction::OptOut
    );
    assert_eq!(
        privacy_action(&privacy_interaction(88, Some("opt-in"))),
        PrivacyAction::OptIn
    );
    assert_eq!(
        privacy_action(&privacy_interaction(88, Some("status"))),
        PrivacyAction::Status
    );
    assert_eq!(
        privacy_action(&privacy_interaction(88, Some("delete"))),
        PrivacyAction::DeletionRequest
    );
    assert_eq!(
        privacy_action(&privacy_interaction(88, Some("deletion-status"))),
        PrivacyAction::DeletionStatus
    );
    assert_eq!(
        privacy_action(&privacy_interaction(88, Some("deletion-receipt"))),
        PrivacyAction::DeletionReceipt
    );
    // Missing options, a non-subcommand value and an unknown name all
    // refuse before any database access.
    assert_eq!(
        privacy_action(&privacy_interaction(88, None)),
        PrivacyAction::Invalid
    );
    let mut flat = slash(
        "privacy",
        Some(CHANNEL),
        vec![option(
            "opt-out",
            CommandOptionValue::String("yes".to_owned()),
        )],
    );
    flat = from_member(flat, 88, Permissions::empty());
    assert_eq!(privacy_action(&flat), PrivacyAction::Invalid);
    assert_eq!(
        privacy_action(&privacy_interaction(88, Some("erase-everything"))),
        PrivacyAction::Invalid
    );
    // A non-command interaction never dispatches.
    let mut component = privacy_interaction(88, Some("opt-out"));
    component.data = None;
    assert_eq!(privacy_action(&component), PrivacyAction::Invalid);
}

#[test]
fn deletion_status_and_receipt_accept_only_a_nested_request_id_string() {
    let request_id = "del-0123456789abcdef";
    let nested = vec![option(
        "deletion-status",
        CommandOptionValue::SubCommand(vec![option(
            "request-id",
            CommandOptionValue::String(request_id.to_owned()),
        )]),
    )];
    let interaction = from_member(
        slash("privacy", Some(CHANNEL), nested),
        88,
        Permissions::empty(),
    );
    assert_eq!(deletion_request_id(&interaction), Some(request_id));
    let unrelated = privacy_request_interaction_in(
        100000000000000431,
        100000000000000432,
        "delete",
        Some(request_id),
    );
    assert_eq!(deletion_request_id(&unrelated), None);

    let malformed = from_member(
        slash(
            "privacy",
            Some(CHANNEL),
            vec![option(
                "deletion-receipt",
                CommandOptionValue::SubCommand(vec![option(
                    "request-id",
                    CommandOptionValue::Integer(123),
                )]),
            )],
        ),
        88,
        Permissions::empty(),
    );
    assert_eq!(deletion_request_id(&malformed), None);
}

/// `/privacy` end to end through gateway dispatch: `opt-out` purges the
/// invoker's guild rows, installs suppression and writes exactly one
/// `member:self-serve` receipt; `status` reports suppression plus the guild
/// switches; `opt-in` clears suppression; the other member's rows survive
/// throughout. Every step defers once and completes with one ephemeral edit.
#[tokio::test]
#[ignore = "requires disposable agent-testdb and mock REST only"]
async fn privacy_command_purges_suppresses_receipts_and_reenables_through_dispatch() {
    use two_bot_cutover::db::{CutoverDb, FunnelWrite};

    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test bootstrap URL");
    let db =
        two_bot_testsupport::TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("migrated test database; no credential fallback");
    let cutover = CutoverDb::from_pool(db.pool().clone());
    // Real-shaped snowflakes: `opt_out_member` validates both identities
    // (17–20 digits), so the small `GUILD`/`88` fixtures would refuse.
    const PRIVACY_GUILD: u64 = 100000000000000411;
    const MEMBER_A: u64 = 100000000000000412;
    const MEMBER_B: u64 = 100000000000000413;
    let guild_s = PRIVACY_GUILD.to_string();
    let member_a_s = MEMBER_A.to_string();
    let member_b_s = MEMBER_B.to_string();
    let seed = |member: &str| FunnelWrite {
        member_id: Some(member.to_owned()),
        guild_id: guild_s.clone(),
        event_type: "first_message".to_owned(),
        occurred_at: "2026-10-12T00:00:00Z".to_owned(),
        source: "privacy-command-fixture".to_owned(),
        metadata: Some("{}".to_owned()),
    };
    for member in [&member_a_s, &member_b_s] {
        let (inserted, _) = two_bot_cutover::db::record_event(&cutover, &seed(member))
            .await
            .expect("seed persists before the opt-out");
        assert!(inserted, "seed row for member {member}");
    }

    let (mock, origin) = MockRest::start(Vec::new()).await;
    let executor = ActionExecutor::with_proxy("mock-only-token".into(), Some(origin)).unwrap();
    let mut privacy_gates = gates(false, false);
    privacy_gates.configured_guild = Some(PRIVACY_GUILD);
    let runtime = CommandRuntime::new(
        db.pool().clone(),
        executor.clone(),
        router_with_commands(privacy_gates),
        PRIVACY_GUILD,
        false,
    );

    let events_for = |member: &str| {
        let pool = db.pool().clone();
        let guild = guild_s.clone();
        let member = member.to_owned();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM events WHERE guild_id = $1 AND member_id = $2",
            )
            .bind(guild)
            .bind(member)
            .fetch_one(&pool)
            .await
            .expect("events count reads")
        }
    };
    let suppressed = |member: &str| {
        let pool = db.pool().clone();
        let guild = guild_s.clone();
        let member = member.to_owned();
        async move {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2)",
            )
            .bind(guild)
            .bind(member)
            .fetch_one(&pool)
            .await
            .expect("suppression reads")
        }
    };

    // `opt-out`: purge + suppress + exactly one receipt, other member kept.
    runtime
        .on_interaction(&privacy_interaction_in(
            PRIVACY_GUILD,
            MEMBER_A,
            Some("opt-out"),
        ))
        .await;
    assert_eq!(
        events_for(&member_a_s).await,
        0,
        "opt-out purges the invoker rows"
    );
    assert_eq!(
        events_for(&member_b_s).await,
        1,
        "other member rows survive"
    );
    assert!(
        suppressed(&member_a_s).await,
        "opt-out installs suppression"
    );
    assert!(
        !suppressed(&member_b_s).await,
        "no suppression for anyone else"
    );
    let receipts: Vec<(String, String, String)> =
        sqlx::query_as("SELECT guild_id, member_id, actor FROM privacy_opt_out_audit")
            .fetch_all(db.pool())
            .await
            .expect("receipt reads");
    assert_eq!(receipts.len(), 1, "exactly one audit receipt");
    assert_eq!(
        receipts[0],
        (
            guild_s.clone(),
            member_a_s.clone(),
            "member:self-serve".to_owned()
        ),
        "receipt scopes the pair with a subject-free actor label"
    );
    // A write that passes after the purge still drops for the pair.
    let (inserted, _) = two_bot_cutover::db::record_event(&cutover, &seed(&member_a_s))
        .await
        .expect("post-purge write attempts");
    assert!(!inserted, "suppressed pair stays dropped");

    // `status`: opted out, guild switches default on.
    runtime
        .on_interaction(&privacy_interaction_in(
            PRIVACY_GUILD,
            MEMBER_A,
            Some("status"),
        ))
        .await;
    // `opt-in`: suppression clears, purged rows stay gone.
    runtime
        .on_interaction(&privacy_interaction_in(
            PRIVACY_GUILD,
            MEMBER_A,
            Some("opt-in"),
        ))
        .await;
    assert!(!suppressed(&member_a_s).await, "opt-in clears suppression");
    assert_eq!(events_for(&member_a_s).await, 0, "opt-in restores nothing");
    // `status` again: persistence back on.
    runtime
        .on_interaction(&privacy_interaction_in(
            PRIVACY_GUILD,
            MEMBER_A,
            Some("status"),
        ))
        .await;
    // Unknown subcommand shape: usage reply, no writes.
    runtime
        .on_interaction(&privacy_interaction_in(PRIVACY_GUILD, MEMBER_A, None))
        .await;
    assert_eq!(
        events_for(&member_b_s).await,
        1,
        "usage path writes nothing"
    );

    // Guild-less invocation refuses on a direct call (dispatch itself stays
    // silent outside the configured guild via the router fence).
    let mut dm = privacy_interaction_in(PRIVACY_GUILD, MEMBER_A, Some("status"));
    dm.guild_id = None;
    crate::privacy_command::privacy_command(db.pool(), &executor, &dm).await;

    let callbacks = mock.posts_to("/callback").await;
    assert_eq!(callbacks.len(), 5, "one defer per dispatched command");
    for callback in &callbacks {
        let body: serde_json::Value = serde_json::from_slice(&callback.body).unwrap();
        assert_eq!(body["type"], 5, "defer before any store work");
        assert_eq!(body["data"]["flags"], 64, "defer stays ephemeral");
    }
    let edits: Vec<serde_json::Value> = mock
        .requests()
        .iter()
        .filter(|r| r.method == "PATCH" && r.path.ends_with("/@original"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(
        edits.len(),
        6,
        "one completion edit per command plus the direct call"
    );
    let text = |index: usize| {
        edits[index]["content"]
            .as_str()
            .expect("edit carries text")
            .to_owned()
    };
    assert!(
        text(0).contains("opted out of analytics"),
        "opt-out confirms, got: {}",
        text(0)
    );
    assert!(
        text(1).contains("opted out") && text(1).contains("presence on"),
        "status reports suppression plus switches, got: {}",
        text(1)
    );
    assert!(
        text(2).contains("back on"),
        "opt-in confirms, got: {}",
        text(2)
    );
    assert!(
        text(3).contains("Analytics for you in this server: on"),
        "status reports restored persistence, got: {}",
        text(3)
    );
    assert!(
        text(4).contains("/privacy opt-out"),
        "unknown shape teaches usage, got: {}",
        text(4)
    );
    assert!(
        text(5).contains("only works in a server"),
        "guild-less call refuses, got: {}",
        text(5)
    );
    for edit in &edits {
        let content = edit["content"].as_str().unwrap_or_default();
        assert!(!content.contains("token-"), "no interaction token echoes");
        assert!(!content.contains("@everyone"), "no mass mentions");
    }
    mock.shutdown().await;
    db.close().await.expect("drop own disposable database");
}

/// `/privacy` deletion commands cross the real gateway dispatcher with the
/// interaction-derived guild/member pair; another member cannot read status or
/// the completed receipt. Completion is synthesized only inside this disposable
/// database so the test does not enable retention execution or run the scheduler.
#[tokio::test]
#[ignore = "requires disposable agent-testdb and mock REST only"]
async fn privacy_deletion_dispatch_binds_request_status_and_receipt_to_invoker() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test bootstrap URL");
    let db =
        two_bot_testsupport::TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("migrated test database; no credential fallback");
    const PRIVACY_GUILD: u64 = 100000000000000431;
    const MEMBER_A: u64 = 100000000000000432;
    const MEMBER_B: u64 = 100000000000000433;
    const DELETE_INTERACTION: u64 = 100000000000000434;
    const NEXT_DELETE_INTERACTION: u64 = 100000000000000435;
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let executor = ActionExecutor::with_proxy("mock-only-token".into(), Some(origin)).unwrap();
    let mut privacy_gates = gates(false, false);
    privacy_gates.configured_guild = Some(PRIVACY_GUILD);
    let runtime = CommandRuntime::new(
        db.pool().clone(),
        executor,
        router_with_commands(privacy_gates),
        PRIVACY_GUILD,
        false,
    );

    let mut delete = privacy_request_interaction_in(PRIVACY_GUILD, MEMBER_A, "delete", None);
    delete.id = Id::new(DELETE_INTERACTION);
    runtime.on_interaction(&delete).await;
    let request: (String, String, String, String) = sqlx::query_as(
        "SELECT request_id, guild_id, user_id, status FROM retention_deletion_requests \
         WHERE guild_id = $1 AND user_id = $2",
    )
    .bind(PRIVACY_GUILD.to_string())
    .bind(MEMBER_A.to_string())
    .fetch_one(db.pool())
    .await
    .expect("live dispatch queued a requester-bound deletion");
    assert_eq!(request.1, PRIVACY_GUILD.to_string());
    assert_eq!(request.2, MEMBER_A.to_string());
    assert_eq!(request.3, "pending");

    runtime
        .on_interaction(&privacy_request_interaction_in(
            PRIVACY_GUILD,
            MEMBER_A,
            "deletion-status",
            Some(&request.0),
        ))
        .await;
    runtime
        .on_interaction(&privacy_request_interaction_in(
            PRIVACY_GUILD,
            MEMBER_B,
            "deletion-status",
            Some(&request.0),
        ))
        .await;

    // Simulate only the settled ledger state needed to exercise receipt reads;
    // no erasure, purge, scheduler, or production database is involved.
    sqlx::query(
        "UPDATE retention_deletion_requests \
         SET status = 'completed', completed_at = clock_timestamp(), receipt_id = 'test-receipt' \
         WHERE request_id = $1",
    )
    .bind(&request.0)
    .execute(db.pool())
    .await
    .expect("fixture marks this disposable-ledger row completed");
    runtime.on_interaction(&delete).await;
    let retried_requests: Vec<(String, String)> = sqlx::query_as(
        "SELECT request_id, status FROM retention_deletion_requests \
         WHERE guild_id = $1 AND user_id = $2",
    )
    .bind(PRIVACY_GUILD.to_string())
    .bind(MEMBER_A.to_string())
    .fetch_all(db.pool())
    .await
    .expect("read ledger after retrying the same interaction");
    assert_eq!(
        retried_requests,
        vec![(request.0.clone(), "completed".to_owned())],
        "same-interaction retry reuses the completed request without inserting a row"
    );
    runtime
        .on_interaction(&privacy_request_interaction_in(
            PRIVACY_GUILD,
            MEMBER_A,
            "deletion-receipt",
            Some(&request.0),
        ))
        .await;
    runtime
        .on_interaction(&privacy_request_interaction_in(
            PRIVACY_GUILD,
            MEMBER_B,
            "deletion-receipt",
            Some(&request.0),
        ))
        .await;

    // Change only the interaction id: a new tap must not reuse the completed
    // request even though the authenticated guild and member are unchanged.
    let mut next_delete = delete.clone();
    next_delete.id = Id::new(NEXT_DELETE_INTERACTION);
    runtime.on_interaction(&next_delete).await;
    let requests: Vec<(String, String)> = sqlx::query_as(
        "SELECT request_id, status FROM retention_deletion_requests \
         WHERE guild_id = $1 AND user_id = $2",
    )
    .bind(PRIVACY_GUILD.to_string())
    .bind(MEMBER_A.to_string())
    .fetch_all(db.pool())
    .await
    .expect("read ledger after a distinct deletion interaction");
    assert_eq!(requests.len(), 2, "a distinct tap queues a second request");
    assert!(
        requests.contains(&(request.0.clone(), "completed".to_owned())),
        "the original completed request remains settled"
    );
    let (next_request_id, next_status) = requests
        .iter()
        .find(|(request_id, _)| request_id != &request.0)
        .expect("the new interaction has a different request id");
    assert_eq!(next_status, "pending");

    let edits: Vec<serde_json::Value> = mock
        .requests()
        .iter()
        .filter(|request| request.method == "PATCH" && request.path.ends_with("/@original"))
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(
        edits.len(),
        7,
        "each dispatched command completes its defer"
    );
    assert!(edits[0]["content"].as_str().unwrap().contains("pending"));
    assert!(edits[1]["content"].as_str().unwrap().contains("pending"));
    assert_eq!(
        edits[2]["content"], "Privacy command failed; try again.",
        "another member cannot read the request status"
    );
    let retry_reply = edits[3]["content"].as_str().unwrap();
    assert!(retry_reply.contains(&request.0));
    assert!(retry_reply.contains("already accepted"));
    assert!(retry_reply.contains("check its current status"));
    assert!(!retry_reply.contains("queued"));
    assert!(edits[4]["content"]
        .as_str()
        .unwrap()
        .contains("erasable rows in the member erasure plan"));
    assert_eq!(
        edits[5]["content"], "Privacy command failed; try again.",
        "another member cannot read the completion receipt"
    );
    let next_reply = edits[6]["content"].as_str().unwrap();
    assert!(next_reply.contains("queued as pending"));
    assert!(next_reply.contains(next_request_id));
    assert!(!next_reply.contains(&request.0));
    mock.shutdown().await;
    db.close().await.expect("drop own disposable database");
}

/// `/privacy opt-out` abuse guards through gateway dispatch: a repeat tap
/// while suppressed returns the already-opted-out reply, and a rapid re-opt
/// right after opt-in returns the cooldown reply. Both early returns skip
/// `opt_out_member` — the only `LOCK TABLE ... SHARE ROW EXCLUSIVE` on this
/// path — so neither writes a new receipt. Backdating the receipt past the
/// cooldown restores the full purge. Receipt count is the observable proxy:
/// every purge writes exactly one receipt, every skipped purge writes none.
#[tokio::test]
#[ignore = "requires disposable agent-testdb and mock REST only"]
async fn privacy_command_repeat_opt_out_skips_purge_and_cooldown_blocks_rapid_reopt() {
    use two_bot_cutover::db::{CutoverDb, FunnelWrite};

    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test bootstrap URL");
    let db =
        two_bot_testsupport::TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("migrated test database; no credential fallback");
    let cutover = CutoverDb::from_pool(db.pool().clone());
    const GUARD_GUILD: u64 = 100000000000000421;
    const GUARD_MEMBER: u64 = 100000000000000422;
    let guild_s = GUARD_GUILD.to_string();
    let member_s = GUARD_MEMBER.to_string();
    let (inserted, _) = two_bot_cutover::db::record_event(
        &cutover,
        &FunnelWrite {
            member_id: Some(member_s.clone()),
            guild_id: guild_s.clone(),
            event_type: "first_message".to_owned(),
            occurred_at: "2026-10-12T00:00:00Z".to_owned(),
            source: "privacy-guard-fixture".to_owned(),
            metadata: Some("{}".to_owned()),
        },
    )
    .await
    .expect("seed persists before the opt-out");
    assert!(inserted, "seed row for the guard member");

    let (mock, origin) = MockRest::start(Vec::new()).await;
    let executor = ActionExecutor::with_proxy("mock-only-token".into(), Some(origin)).unwrap();
    let mut privacy_gates = gates(false, false);
    privacy_gates.configured_guild = Some(GUARD_GUILD);
    let runtime = CommandRuntime::new(
        db.pool().clone(),
        executor.clone(),
        router_with_commands(privacy_gates),
        GUARD_GUILD,
        false,
    );

    async fn receipts(pool: &sqlx::Pool<sqlx::Postgres>, guild: &str, member: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM privacy_opt_out_audit WHERE guild_id = $1 AND member_id = $2",
        )
        .bind(guild)
        .bind(member)
        .fetch_one(pool)
        .await
        .expect("receipt reads")
    }

    async fn suppressed(pool: &sqlx::Pool<sqlx::Postgres>, guild: &str, member: &str) -> bool {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2)",
        )
        .bind(guild)
        .bind(member)
        .fetch_one(pool)
        .await
        .expect("suppression reads")
    }

    // First purge: one receipt, suppression installed.
    runtime
        .on_interaction(&privacy_interaction_in(
            GUARD_GUILD,
            GUARD_MEMBER,
            Some("opt-out"),
        ))
        .await;
    assert_eq!(receipts(db.pool(), &guild_s, &member_s).await, 1);
    assert!(suppressed(db.pool(), &guild_s, &member_s).await);

    // Repeat tap while suppressed: no new receipt, suppression stays.
    runtime
        .on_interaction(&privacy_interaction_in(
            GUARD_GUILD,
            GUARD_MEMBER,
            Some("opt-out"),
        ))
        .await;
    assert_eq!(
        receipts(db.pool(), &guild_s, &member_s).await,
        1,
        "repeat opt-out writes no new receipt, so no purge ran"
    );
    assert!(suppressed(db.pool(), &guild_s, &member_s).await);

    // Opt back in clears suppression without a receipt.
    runtime
        .on_interaction(&privacy_interaction_in(
            GUARD_GUILD,
            GUARD_MEMBER,
            Some("opt-in"),
        ))
        .await;
    assert!(!suppressed(db.pool(), &guild_s, &member_s).await);
    assert_eq!(receipts(db.pool(), &guild_s, &member_s).await, 1);

    // Rapid re-opt inside the cooldown: blocked, still no receipt and no
    // suppression reinstall.
    runtime
        .on_interaction(&privacy_interaction_in(
            GUARD_GUILD,
            GUARD_MEMBER,
            Some("opt-out"),
        ))
        .await;
    assert_eq!(
        receipts(db.pool(), &guild_s, &member_s).await,
        1,
        "cooldown re-opt writes no new receipt, so no purge ran"
    );
    assert!(
        !suppressed(db.pool(), &guild_s, &member_s).await,
        "cooldown path installs no suppression"
    );

    // Past the cooldown the purge runs again: backdate the receipt, then a
    // fresh opt-out receipts and suppresses.
    sqlx::query(
        "UPDATE privacy_opt_out_audit
         SET opted_out_at = clock_timestamp() - make_interval(mins => 11)
         WHERE guild_id = $1 AND member_id = $2",
    )
    .bind(&guild_s)
    .bind(&member_s)
    .execute(db.pool())
    .await
    .expect("backdate writes");
    runtime
        .on_interaction(&privacy_interaction_in(
            GUARD_GUILD,
            GUARD_MEMBER,
            Some("opt-out"),
        ))
        .await;
    assert_eq!(
        receipts(db.pool(), &guild_s, &member_s).await,
        2,
        "post-cooldown opt-out purges again"
    );
    assert!(suppressed(db.pool(), &guild_s, &member_s).await);

    let edits: Vec<serde_json::Value> = mock
        .requests()
        .iter()
        .filter(|r| r.method == "PATCH" && r.path.ends_with("/@original"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(edits.len(), 5, "one completion edit per command");
    let text = |index: usize| {
        edits[index]["content"]
            .as_str()
            .expect("edit carries text")
            .to_owned()
    };
    assert!(
        text(0).contains("opted out of analytics"),
        "first purge confirms, got: {}",
        text(0)
    );
    assert!(
        text(1).contains("already opted out"),
        "repeat tap reports suppression, got: {}",
        text(1)
    );
    assert!(
        text(2).contains("back on"),
        "opt-in confirms, got: {}",
        text(2)
    );
    assert!(
        text(3).contains("recently"),
        "cooldown asks to retry later, got: {}",
        text(3)
    );
    assert!(
        text(4).contains("opted out of analytics"),
        "post-cooldown purge confirms, got: {}",
        text(4)
    );
    mock.shutdown().await;
    db.close().await.expect("drop own disposable database");
}

// ---------------------------------------------------------------------------
// Stale-control review pins: voice-epoch/ballot silence, cross-channel stale
// replies, and split bot-author arms. The protected acceptance module owns the
// normative suite; these pin the review fixes without touching it.
// ---------------------------------------------------------------------------

fn stale_unknown_component(
    custom_id: &str,
    component_type: ComponentType,
    values: Vec<String>,
) -> Interaction {
    let mut interaction = slash("unused", Some(CHANNEL), Vec::new());
    interaction.kind = InteractionType::MessageComponent;
    interaction.data = Some(InteractionData::MessageComponent(Box::new(
        MessageComponentInteractionData {
            custom_id: custom_id.to_owned(),
            component_type,
            resolved: None,
            values,
        },
    )));
    interaction
}

fn assert_one_expired_reply(requests: &[RestRequest]) {
    use two_bot_core::router::replies::EXPIRED_COMPONENT_REPLY;

    assert_eq!(requests.len(), 1, "one callback, zero other effects");
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert!(
        request.path.ends_with("/callback"),
        "callback path: {}",
        request.path
    );
    let reply: serde_json::Value = serde_json::from_slice(&request.body).expect("reply json");
    assert_eq!(reply["type"], 4, "immediate response, not a defer");
    assert_eq!(reply["data"]["flags"], 64, "ephemeral");
    assert_eq!(reply["data"]["content"], EXPIRED_COMPONENT_REPLY);
}

#[tokio::test]
async fn epoch_suffixed_voice_join_stays_silent() {
    use two_bot_core::voice_custom_id::join_custom_id;
    use two_bot_core::voice_private::JoinDecision;

    let room = 777_000_111_222_333_444u64;
    let request = 888_000_111_222_333_444u64;
    // Production runtime shape, not the bare codec id.
    let custom_id = format!(
        "{}:0123456789abcdef0123456789abcdef",
        join_custom_id(JoinDecision::Approve, room, request)
    );
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    let button = stale_unknown_component(&custom_id, ComponentType::Button, Vec::new());
    runtime.on_interaction(&button).await;
    assert!(
        mock.requests().is_empty(),
        "epoch-suffixed join belongs to the voice sink: silence"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn votekick_ballot_buttons_stay_silent() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    for custom_id in [
        "votekick:7000000111222333444:yes",
        "votekick:7000000111222333444:no",
    ] {
        let button = stale_unknown_component(custom_id, ComponentType::Button, Vec::new());
        runtime.on_interaction(&button).await;
    }
    assert!(
        mock.requests().is_empty(),
        "ballots belong to the voice sink: silence"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn bot_only_on_member_user_stays_silent() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    let mut button = stale_unknown_component(
        "two:stale-widget:deadbeef",
        ComponentType::Button,
        Vec::new(),
    );
    button
        .member
        .as_mut()
        .expect("guild member")
        .user
        .as_mut()
        .expect("member user")
        .bot = true;
    assert!(
        !button.user.as_ref().expect("top-level user").bot,
        "top-level user stays non-bot: pins the member arm alone"
    );
    runtime.on_interaction(&button).await;
    assert!(
        mock.requests().is_empty(),
        "bot on member.user only stays silent"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn bot_only_on_top_level_user_stays_silent() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(gates(true, true), true, origin);
    let mut button = stale_unknown_component(
        "two:stale-widget:deadbeef",
        ComponentType::Button,
        Vec::new(),
    );
    button.user.as_mut().expect("top-level user").bot = true;
    assert!(
        !button
            .member
            .as_ref()
            .expect("guild member")
            .user
            .as_ref()
            .expect("member user")
            .bot,
        "member.user stays non-bot: pins the top-level arm alone"
    );
    runtime.on_interaction(&button).await;
    assert!(
        mock.requests().is_empty(),
        "bot on top-level user only stays silent"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn cross_channel_replaced_self_role_gets_expired_reply() {
    use std::collections::HashSet;
    use two_bot_core::self_roles::{
        self_role_custom_id, PanelMode, SelfRoleGates, SelfRoleOption, SelfRolePanel,
    };
    use two_bot_cutover::self_role_store::SelfRoleStore;

    use crate::self_role_handlers::SelfRoleService;
    use crate::self_role_runtime::SelfRoleRuntime;

    const SR_GUILD: u64 = 100_000_000_000_000_001;
    const OLD_CHANNEL: u64 = 100_000_000_000_000_007;
    const OLD_MESSAGE: u64 = 100_000_000_000_000_008;
    const NEW_CHANNEL: &str = "100000000000000017";
    const NEW_MESSAGE: &str = "100000000000000018";
    const SR_USER: u64 = 100_000_000_000_000_002;
    const SR_BOT: &str = "100000000000000003";
    const SR_EVENT: u64 = 100_000_000_000_000_011;

    let (mock, origin) = MockRest::start(Vec::new()).await;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    let executor = ActionExecutor::with_proxy("test-token".to_owned(), Some(origin.clone()))
        .expect("mock executor");
    // The panel moved to a new channel and message; the old source survives.
    let service = Arc::new(
        SelfRoleService::new(
            SelfRoleRuntime {
                store: SelfRoleStore::new(pool.clone()),
                executor: executor.clone(),
                guild_id: SR_GUILD.to_string(),
                bot_id: SR_BOT.to_owned(),
            },
            SelfRoleGates {
                panels: vec![SelfRolePanel {
                    id: "games".into(),
                    channel_id: NEW_CHANNEL.into(),
                    message_id: NEW_MESSAGE.into(),
                    mode: PanelMode::Button,
                    exclusive: false,
                    color: false,
                    options: vec![SelfRoleOption {
                        key: "new".into(),
                        label: "New".into(),
                        role_id: "100000000000000004".into(),
                        permissions: "0".into(),
                        emoji: None,
                        description: None,
                    }],
                }],
                dry_run: false,
            },
            &[SR_GUILD.to_string()].into_iter().collect::<HashSet<_>>(),
        )
        .expect("self-role service"),
    );
    let runtime = CommandRuntime::new_with_self_roles(
        pool,
        executor,
        router_with_commands(RouterGates {
            configured_guild: Some(SR_GUILD),
            scorecard: false,
            automations: true,
            announcements: true,
            moderation: false,
            voice: false,
            voice_assistant: false,
            tickets: false,
            self_roles: true,
            onboarding_picker: false,
            session_picker: false,
        }),
        SR_GUILD,
        true,
        service,
    );
    // A press on the old panel message in the old channel.
    let mut press = slash("unused", Some(OLD_CHANNEL), Vec::new());
    press.guild_id = Some(Id::new(SR_GUILD));
    press.id = Id::new(SR_EVENT);
    press.kind = InteractionType::MessageComponent;
    press
        .member
        .as_mut()
        .expect("guild member")
        .user
        .as_mut()
        .expect("member user")
        .id = Id::new(SR_USER);
    let mut source = message(1, OLD_CHANNEL, false, Some(SR_GUILD));
    source.id = Id::new(OLD_MESSAGE);
    press.message = Some(source);
    press.data = Some(InteractionData::MessageComponent(Box::new(
        MessageComponentInteractionData {
            custom_id: self_role_custom_id("games", Some("new")),
            component_type: ComponentType::Button,
            resolved: None,
            values: Vec::new(),
        },
    )));
    runtime.on_interaction(&press).await;
    assert_one_expired_reply(&mock.requests());
    mock.shutdown().await;
}
