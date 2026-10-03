//! Staging-smoke error-contract fixture: every documented command-failure
//! path yields exactly one triage outcome (one watch-log error class or an
//! explicit none) and one fixed user-facing reply.
//!
//! The staging smoke harness runs core slash commands and must classify a red
//! run against the 48-hour watch log, which accepts only the fixed
//! error-class vocabulary in `docs/production-deploy.md` (sourced from
//! `docs/startup-diagnostics.md`). This module pins the decision table the
//! harness triages with, driving each path through the real
//! [`CommandRuntime::on_interaction`] over a loopback REST double and a lazy
//! pool that can never connect (the same never-DB shape as
//! `command_runtime_tests::runtime_without_db`):
//!
//! | Smoke path | Watch-log class | User reply (exact) |
//! | --- | --- | --- |
//! | unknown command | none — expected refusal, not an error | `I don't recognize that command. ...` ([`UNKNOWN_COMMAND_REPLY`], documented in `docs/interaction-replies.md`) |
//! | permission denied | none — the harness identity lacks rights, the bot is healthy | `You need the Manage Server permission ...` ([`RouterRefusal::ManageServerRequired`]) |
//! | store unavailable | `store_unavailable` — the only path that is a watch-log error | `Feed command failed; try again.` (safe text, never sqlx internals) |
//! | invalid input | none — the harness sent a bad argument | `Unknown feed kind.` ([`FeedError::InvalidKind`]) |
//!
//! Only the store path carries a watch-log class. Mapping the three expected
//! refusals onto startup/infrastructure classes would file watch-log rows for
//! healthy-bot outcomes and make a red smoke run untriagable, which is what
//! this fixture exists to prevent. The `None` arms are therefore asserted
//! explicitly, not left as gaps: each path maps to exactly one outcome, and
//! every class present is a member of the fixed vocabulary, never raw text.

#![cfg(test)]

use std::sync::{Arc, Mutex};

use sqlx::postgres::PgPoolOptions;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use twilight_model::application::command::CommandType;
use twilight_model::application::interaction::application_command::{
    CommandData, CommandDataOption, CommandOptionValue,
};
use twilight_model::application::interaction::{Interaction, InteractionData, InteractionType};
use twilight_model::guild::{MemberFlags, Permissions};
use twilight_model::id::{AnonymizableId, Id};
use twilight_model::oauth::ApplicationIntegrationMap;
use twilight_model::user::User;
use two_bot_core::feeds::FeedError;
use two_bot_core::router::replies::UNKNOWN_COMMAND_REPLY;
use two_bot_core::{RouterGates, RouterRefusal};
use two_bot_discord::ActionExecutor;

use crate::command_runtime::{router_with_commands, CommandRuntime};
use crate::gateway_failure::FailureClass;

const GUILD: u64 = 2222;
const CHANNEL: u64 = 3333;

/// Fixed error-class vocabulary for the 48-hour watch log, transcribed from
/// `docs/production-deploy.md` (`error-class: fixed vocabulary only`, sourced
/// from `docs/startup-diagnostics.md`). The smoke harness may file a watch-log
/// row only with one of these tokens — never raw text, URLs or credentials.
const WATCH_LOG_VOCABULARY: [&str; 16] = [
    "listener_bind_failed",
    "gateway_override_invalid",
    "database_connect_failed",
    "store_unavailable",
    "gateway_pool_connect_failed",
    "checkpoint_load_failed",
    "onboarding_gates_invalid",
    "onboarding_init_failed",
    "milestones_load_failed",
    "automod_config_invalid",
    "automod_executor_failed",
    "gateway_runtime_failed",
    "gateway_task_panicked",
    "container_service_failed",
    "container_lifecycle_failed",
    "container_unavailable",
];

/// One documented command-failure path of the smoke harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SmokePath {
    UnknownCommand,
    PermissionDenied,
    StoreUnavailable,
    InvalidInput,
}

impl SmokePath {
    const ALL: [Self; 4] = [
        Self::UnknownCommand,
        Self::PermissionDenied,
        Self::StoreUnavailable,
        Self::InvalidInput,
    ];

    /// The watch-log error class for this path, if any. Exactly one outcome
    /// per path: `Some` only where the bot itself failed (the store path),
    /// `None` where the outcome is an expected user refusal on a healthy bot.
    fn error_class(self) -> Option<&'static str> {
        match self {
            Self::StoreUnavailable => Some(FailureClass::StoreUnavailable.as_str()),
            Self::UnknownCommand | Self::PermissionDenied | Self::InvalidInput => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Loopback REST double (compact mirror of the command-runtime suite's mock;
// that file is dev-only inside this binary's test tree, so the scripted TCP
// double is reproduced here to keep this fixture independent).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RestRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

struct MockRest {
    recorded: Arc<Mutex<Vec<RestRequest>>>,
    handle: JoinHandle<()>,
}

impl MockRest {
    async fn start() -> (Self, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock listen");
        let origin = format!("http://{}", listener.local_addr().expect("addr"));
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let rec = recorded.clone();
        let handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let rec = rec.clone();
                tokio::spawn(async move {
                    let Some(request) = read_rest_request(&mut stream).await else {
                        return;
                    };
                    rec.lock().expect("recorded").push(RestRequest {
                        method: request.0.clone(),
                        path: request.1.clone(),
                        body: request.2.clone(),
                    });
                    // Callbacks and original-response edits must carry a
                    // nonzero snowflake `id`: the executor's
                    // `mutation_receipt_id` rejects the mutation otherwise
                    // and the runtime never reaches its completion edit.
                    let body = if request.1.ends_with("/callback")
                        || request.1.ends_with("/messages/@original")
                    {
                        "{\"id\":\"99\"}".to_owned()
                    } else {
                        "{}".to_owned()
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
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

    async fn shutdown(self) {
        self.handle.abort();
    }
}

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
// Fixtures: real interactions through the real runtime, never a database.
// ---------------------------------------------------------------------------

fn gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
        scorecard: false,
        automations: true,
        announcements: true,
        moderation: false,
        voice: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
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

#[allow(deprecated)]
fn slash(name: &str, options: Vec<CommandDataOption>) -> Interaction {
    Interaction {
        app_permissions: None,
        application_id: Id::new(1111),
        authorizing_integration_owners: ApplicationIntegrationMap {
            guild: Some(AnonymizableId::Id(Id::new(GUILD))),
            user: None,
        },
        channel: None,
        channel_id: Some(Id::new(CHANNEL)),
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
            user: Some(user(88)),
        }),
        message: None,
        token: "sticky-test-token".to_owned(),
        user: Some(user(99)),
    }
}

fn option(name: &str, value: CommandOptionValue) -> CommandDataOption {
    CommandDataOption {
        name: name.to_owned(),
        value,
    }
}

/// Runtime over a lazy pool that can never connect: any store read fails fast
/// with connection refused, which is exactly the `store_unavailable` shape the
/// smoke harness must classify. No test database is touched.
fn runtime_without_db(origin: String) -> Arc<CommandRuntime> {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    let executor =
        ActionExecutor::with_proxy("test-token".to_owned(), Some(origin)).expect("mock executor");
    CommandRuntime::new(pool, executor, router_with_commands(gates()), GUILD, true)
}

/// The user text of an immediate (type 4) refusal callback. Asserts the
/// envelope too: exactly one request, an immediate ephemeral response, no
/// defer and no other REST effects.
fn immediate_content(requests: &[RestRequest]) -> String {
    assert_eq!(requests.len(), 1, "one callback, no other REST effects");
    assert_eq!(requests[0].method, "POST");
    assert_eq!(
        requests[0].path,
        "/api/v10/interactions/7/sticky-test-token/callback"
    );
    let json: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("callback json");
    assert_eq!(json["type"], 4, "immediate response, not a defer");
    assert_eq!(json["data"]["flags"], 64, "ephemeral");
    json["data"]["content"]
        .as_str()
        .expect("text reply")
        .to_owned()
}

/// The user text of a deferred completion: one type-5 defer, then exactly one
/// PATCH of the original response.
fn deferred_content(requests: &[RestRequest]) -> String {
    assert_eq!(requests.len(), 2, "defer plus one completion edit");
    let json: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("callback json");
    assert_eq!(json["type"], 5, "ephemeral defer first");
    assert_eq!(requests[1].method, "PATCH");
    assert_eq!(
        requests[1].path,
        "/api/v10/webhooks/1111/sticky-test-token/messages/@original"
    );
    let reply: serde_json::Value = serde_json::from_slice(&requests[1].body).expect("edit json");
    reply["content"].as_str().expect("text reply").to_owned()
}

// ---------------------------------------------------------------------------
// Driven contract: one path, one outcome, one reply.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_command_reply_matches_the_documented_text() {
    let (mock, origin) = MockRest::start().await;
    let runtime = runtime_without_db(origin);
    runtime
        .on_interaction(&slash("not-a-command", Vec::new()))
        .await;
    let content = immediate_content(&mock.requests());
    assert_eq!(
        content,
        "I don't recognize that command. It may have been removed or renamed — pick it again from the / command list."
    );
    assert_eq!(content, UNKNOWN_COMMAND_REPLY);
    mock.shutdown().await;
}

#[tokio::test]
async fn permission_denied_reply_matches_the_router_refusal() {
    let (mock, origin) = MockRest::start().await;
    let runtime = runtime_without_db(origin);
    let mut interaction = slash(
        "feed-add",
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
    let content = immediate_content(&mock.requests());
    assert_eq!(
        content,
        "You need the Manage Server permission to use this command. Ask a server admin to grant it."
    );
    assert_eq!(content, RouterRefusal::ManageServerRequired.message());
    mock.shutdown().await;
}

#[tokio::test]
async fn store_unavailable_reply_is_fixed_text_without_internals() {
    let (mock, origin) = MockRest::start().await;
    let runtime = runtime_without_db(origin);
    runtime
        .on_interaction(&slash("feed-list", Vec::new()))
        .await;
    let content = deferred_content(&mock.requests());
    // Safe reply owned by the command runtime: fixed text, never the sqlx
    // error, URL or pool address the pool failure carries.
    assert_eq!(content, "Feed command failed; try again.");
    for leaked in ["sqlx", "postgres", "127.0.0.1", "agent_test"] {
        assert!(
            !content.contains(leaked),
            "store failure must not leak {leaked}"
        );
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn invalid_input_reply_matches_the_domain_text() {
    let (mock, origin) = MockRest::start().await;
    let runtime = runtime_without_db(origin);
    runtime
        .on_interaction(&slash(
            "feed-add",
            vec![
                option("kind", CommandOptionValue::String("atom".to_owned())),
                option(
                    "source",
                    CommandOptionValue::String("https://example.com/feed.xml".to_owned()),
                ),
            ],
        ))
        .await;
    let content = deferred_content(&mock.requests());
    assert_eq!(content, "Unknown feed kind.");
    assert_eq!(content, FeedError::InvalidKind.to_string());
    mock.shutdown().await;
}

// ---------------------------------------------------------------------------
// Classification: exactly one outcome per path, classes from the fixed set.
// ---------------------------------------------------------------------------

#[test]
fn each_path_yields_exactly_one_triage_outcome() {
    // Total: every documented path has a mapping, none has two.
    assert_eq!(SmokePath::ALL.len(), 4);
    let mut seen = std::collections::BTreeSet::new();
    for path in SmokePath::ALL {
        assert!(seen.insert(format!("{path:?}")), "each path listed once");
        // Exactly one outcome: one Option, never a list and never ambiguous.
        let outcome = path.error_class();
        assert_eq!(outcome.is_some(), path == SmokePath::StoreUnavailable);
        if let Some(class) = outcome {
            assert!(
                WATCH_LOG_VOCABULARY.contains(&class),
                "{path:?} class {class} must come from the fixed watch-log vocabulary"
            );
        }
    }
}

#[test]
fn store_class_spelling_matches_the_watch_log() {
    // The enum token is the refinement of the documented row: the gateway
    // task's "shared store missing" step in `docs/startup-diagnostics.md`.
    assert_eq!(FailureClass::StoreUnavailable.as_str(), "store_unavailable");
    assert!(WATCH_LOG_VOCABULARY.contains(&"store_unavailable"));
}

#[test]
fn watch_log_vocabulary_stays_fixed_tokens() {
    // The Worker gate and `scripts/staging_rollout.py` accept only
    // `[a-z0-9_]{1,32}` tokens; a class outside that shape (or a duplicate)
    // would break the watch-log pipeline the smoke run triages against.
    let mut seen = std::collections::BTreeSet::new();
    for token in WATCH_LOG_VOCABULARY {
        assert!(
            (1..=32).contains(&token.len())
                && token
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "vocabulary token shape: {token}"
        );
        assert!(seen.insert(token), "duplicate vocabulary token: {token}");
    }
}

#[test]
fn failure_replies_are_distinct_per_path() {
    // Triage reads the user text back to the path: four paths sharing one
    // reply would make a red smoke line ambiguous.
    let replies = [
        UNKNOWN_COMMAND_REPLY,
        &RouterRefusal::ManageServerRequired.message(),
        "Feed command failed; try again.",
        &FeedError::InvalidKind.to_string(),
    ];
    let mut seen = std::collections::BTreeSet::new();
    for reply in replies {
        assert!(!reply.is_empty());
        assert!(seen.insert(reply), "replies must differ: {reply}");
    }
}
