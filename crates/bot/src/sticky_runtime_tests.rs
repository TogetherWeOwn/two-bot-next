//! TOG-10309 acceptance tests for the sticky runtime wiring.
//!
//! Two layers, mirroring `gateway_tests.rs` + `interaction_routing.rs`:
//!
//! 1. In-memory units (always run): option extraction, reply shape, actor
//!    resolution, gate short-circuits, router-refusal answers, and the
//!    detached `dispatch` path — all through a compact mock REST double.
//! 2. `#[ignore]` agent-testdb acceptance: burst coalescing to one re-post,
//!    previous-sticky retirement, `/sticky-remove` clearing state + message,
//!    claim collision, debounce hold, and REST-failure cleanup. These run in
//!    CI via `TWO_GATEWAY_TEST_DATABASE_URL` (same approved service as the
//!    gateway suite — never the runtime DATABASE_URL).

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
use twilight_model::gateway::payload::incoming::InteractionCreate;
use twilight_model::guild::{MemberFlags, Permissions};
use twilight_model::http::interaction::InteractionResponseType;
use twilight_model::id::{AnonymizableId, Id};
use twilight_model::oauth::ApplicationIntegrationMap;
use twilight_model::user::User;
use twilight_model::util::Timestamp;
use two_bot_core::funnel::now_millis_for_test;
use two_bot_core::sticky::{store, ActivityOutcome};
use two_bot_core::{RouterGates, AUTOMATIONS_DISABLED_REPLY};
use two_bot_discord::ActionExecutor;

use crate::sticky_runtime::{
    actor_id, ephemeral, router_with_sticky, sticky_options, StickyRuntime,
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

fn gates(automations: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
        scorecard: false,
        automations,
        announcements: false,
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
    automations: bool,
    gate_automations: bool,
    origin: String,
) -> Arc<StickyRuntime> {
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    let executor =
        ActionExecutor::with_proxy("test-token".to_owned(), Some(origin)).expect("mock executor");
    StickyRuntime::new(
        pool,
        executor,
        router_with_sticky(gates(gate_automations)),
        GUILD,
        automations,
    )
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
        let runtime = runtime_without_db(automations, true, origin.clone());
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
    let runtime = runtime_without_db(false, false, origin);
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
async fn non_sticky_automation_admin_names_are_ignored() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    // Other slices own both their accepted commands and their refusals.
    for enabled in [true, false] {
        let runtime = runtime_without_db(enabled, enabled, origin.clone());
        for name in ["command", "command-remove", "schedule", "ban", "attendance"] {
            let mut interaction = slash(name, Some(CHANNEL), Vec::new());
            runtime.on_interaction(&interaction).await;
            interaction.member.as_mut().unwrap().permissions = Some(Permissions::empty());
            runtime.on_interaction(&interaction).await;
            interaction.guild_id = Some(Id::new(9999));
            runtime.on_interaction(&interaction).await;
        }
    }
    assert!(
        mock.requests().is_empty(),
        "non-sticky names must not reach Discord"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn dispatch_spawns_interaction_work_off_the_shard_loop() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = runtime_without_db(false, false, origin);
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
}

async fn db_runtime(db: &TestDb, script: Vec<u16>) -> (Arc<StickyRuntime>, MockRest) {
    db_runtime_script(db, script.into_iter().map(RestResponse::status).collect()).await
}

async fn db_runtime_script(
    db: &TestDb,
    script: Vec<RestResponse>,
) -> (Arc<StickyRuntime>, MockRest) {
    let (mock, origin) = MockRest::start_script(script).await;
    let runtime = StickyRuntime::new(
        db.pool.clone(),
        executor_at(origin),
        router_with_sticky(gates(true)),
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
