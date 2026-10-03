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
use crate::schedule_runtime::{schedule_options, schedule_remove_option};

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
                    "sticky"
                        | "sticky-remove"
                        | "feed-add"
                        | "feed-remove"
                        | "feed-list"
                        | "purge"
                        | "slowmode"
                        | "lockdown"
                        | "unlock"
                        | "schedule"
                        | "schedule-remove"
                        | "schedule-list"
                        | "rank"
                        | "leaderboard"
                )
            })
            .collect();
        assert!(!unwired.contains(&"rank"));
        assert!(!unwired.contains(&"leaderboard"));
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
        two_bot_core::router::replies::UNKNOWN_INTERACTION_REPLY
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

#[tokio::test]
async fn bounded_dispatch_keeps_publication_independent_and_cancels_on_gateway_exit() {
    let (mock, origin) = MockRest::start_script(
        (0..17)
            .map(|_| RestResponse {
                status: 200,
                body: None,
                delay: Duration::from_secs(30),
            })
            .collect(),
    )
    .await;
    let runtime = runtime_without_db(gates(false, false), false, origin);
    let guard = runtime.dispatch_guard();
    let event = Event::InteractionCreate(Box::new(InteractionCreate(slash(
        "sticky-remove",
        Some(CHANNEL),
        Vec::new(),
    ))));
    for _ in 0..16 {
        assert!(runtime.dispatch(&event));
    }
    for _ in 0..100 {
        assert!(
            !runtime.dispatch(&event),
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
    wait_for(|| mock.requests().len() == 17, "all admitted work started").await;
    assert_eq!(mock.posts_to("/callback").await.len(), 16);
    drop(guard);
    wait_for(
        || Arc::strong_count(&runtime) == 1,
        "all scoped work cancelled",
    )
    .await;
    assert!(!runtime.dispatch(&event), "closed scope refuses new work");
    assert_eq!(mock.requests().len(), 17, "rejected work never sent HTTP");
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

#[tokio::test]
#[ignore = "requires disposable agent-testdb and mock REST only"]
async fn shared_runtime_routes_leveling_once_with_legacy_visibility_and_guild_fence() {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit test bootstrap URL");
    let db =
        two_bot_testsupport::TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("migrated test database; no credential fallback");
    let (mock, origin) = MockRest::start(vec![204; 4]).await;
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
    for member in 77..89 {
        sqlx::query("INSERT INTO member_levels (guild_id,member_id,xp,imported_xp,updated_at) VALUES ($1,$2,$3,$3,NOW())")
            .bind(GUILD_S).bind(member.to_string()).bind(i64::from(member))
            .execute(db.pool()).await.unwrap();
    }
    runtime.on_interaction(&leaderboard).await;
    for name in ["rank", "leaderboard"] {
        let mut foreign = slash(name, Some(CHANNEL), Vec::new());
        foreign.guild_id = Some(Id::new(9999));
        runtime.on_interaction(&foreign).await;
    }
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        4,
        "exactly one callback per accepted command"
    );
    assert!(requests
        .iter()
        .all(|r| r.method == "POST" && r.path.ends_with("/callback")));
    let replies: Vec<serde_json::Value> = requests
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    for reply in &replies {
        assert_eq!(reply["type"], 4, "no generic defer or second response");
    }
    for (reply, name) in replies[..2].iter().zip(["member", "Target member"]) {
        assert_eq!(reply["data"]["flags"], 64, "rank is ephemeral");
        assert_eq!(
            reply["data"]["content"],
            two_bot_core::leveling::rank_text(name, 0, None, 0, 0)
        );
    }
    assert_eq!(replies[2]["data"]["content"], "No XP has been earned yet.");
    for reply in &replies[2..] {
        assert!(reply["data"]["flags"].is_null(), "leaderboard stays public");
        assert_eq!(
            reply["data"]["allowed_mentions"]["parse"],
            serde_json::json!([])
        );
        assert!(reply["data"]["components"].is_null(), "no invented paging");
    }
    assert_eq!(
        replies[3]["data"]["content"]
            .as_str()
            .unwrap()
            .lines()
            .count(),
        11
    );
    assert!(!replies[3]["data"]["content"]
        .as_str()
        .unwrap()
        .contains("<@77>"));
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
    assert_eq!(audits[0].0, "slowmode");
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
    assert_eq!(removed["content"], "Cancelled.");
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
