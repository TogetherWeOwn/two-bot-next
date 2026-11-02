//! Protected behavioral acceptance: every member-authority refusal carries its cause.
//!
//! Each `AuthorityUnavailable` refusal must log exactly one `warn!` carrying a
//! fixed cause token, while the `moderation_enforcement` metric keeps counting
//! the refusal and the member-facing reply stays generic. The builder implements
//! against this file and must never edit it; test corrections go to the author.
//!
//! Contract pinned here (RED-first: the `warn!` does not exist yet, so every
//! cause test fails today on the log assertion while the reply, metric and
//! audit controls pass):
//! - Message: the refusal line contains `member authority unavailable`.
//! - Tokens (one per cause): `discord_403`, `transport`, `malformed_roles`,
//!   `unknown_bot`, `guild_mismatch`. The token may sit in a `cause` field or
//!   in the message text; the tests match substrings, never a full format.
//! - Metric: `two_bot_moderation_enforcements_total{verb,outcome="refused"}`
//!   grows by exactly one per refusal. The builder may add the cause as a
//!   bounded label; the tests match `verb` + `outcome` in any label order.
//! - Reply: the completion edit is exactly
//!   `Moderation command failed; try again.` and names no cause token.
//! - One line per refusal, zero on success. Faults this file does not trigger
//!   (bot identity present but not a bot, unknown held roles, 429s, invalid
//!   JSON bodies, non-403 4xx) map to the nearest token; the builder pins them
//!   with its own unit tests so no refusal stays cause-blind.
//!
//! The tests drive `handle_command` directly instead of the shared router on
//! purpose: the router fences the guild before the member consumer runs, so a
//! guild-mismatched interaction only reaches the runtime check this way.
//! No live Discord (loopback mock REST) and no application database (a fresh
//! disposable `agent-testdb` slice per drive, same as the sibling dispatch
//! acceptance). The ignored runtime lane runs these serially; the tests still
//! hold `MODERATION_METRICS_LOCK` and slice the process-wide capture by offset.

#![cfg(test)]

use super::*;
use serde_json::{json, Value};
use two_bot_core::moderation::ModerationAction;
use two_bot_testsupport::TestDatabase;

use crate::member_runtime::handle_command;

/// Fixed refusal-line marker. The builder's `warn!` must contain this text.
const REFUSAL_LINE: &str = "member authority unavailable";
/// The generic member-facing completion. Literal copy, not the runtime const:
/// any cause leak or rewording must fail.
const GENERIC_REPLY: &str = "Moderation command failed; try again.";
/// Fixed cause tokens, one per refusal cause below.
const CAUSES: [&str; 5] = [
    "discord_403",
    "transport",
    "malformed_roles",
    "unknown_bot",
    "guild_mismatch",
];

const ACTOR_ID: u64 = 88;
const TARGET_ID: u64 = 3333;
const BOT_ID: u64 = 4444;
const OWNER_ID: &str = "5555";
const MODERATOR_ROLE: &str = "101";
const BOT_ROLE: &str = "103";
const OTHER_GUILD: u64 = 9999;

fn member_body(id: u64, roles: &[&str], bot: bool) -> Value {
    json!({
        "user": user(id, bot),
        "roles": roles,
        "deaf": false,
        "mute": false,
        "joined_at": "2026-10-01T00:00:00Z",
    })
}

fn healthy_guild() -> Value {
    let moderator_permissions =
        (Permissions::BAN_MEMBERS | Permissions::KICK_MEMBERS | Permissions::MODERATE_MEMBERS)
            .bits()
            .to_string();
    json!({
        "id": GUILD_S,
        "owner_id": OWNER_ID,
        "roles": [
            {"id": GUILD_S, "position": 0, "permissions": "0"},
            {"id": MODERATOR_ROLE, "position": 50, "permissions": moderator_permissions},
            {"id": BOT_ROLE, "position": 100, "permissions": Permissions::all().bits().to_string()},
        ],
    })
}

/// Scripted authority fixtures. Every route serves healthy evidence unless a
/// fault overrides it, so a refusal proves the fault caused it.
#[derive(Clone)]
struct CauseFacts {
    identity_status: u16,
    guild_status: u16,
    guild_body: Value,
    actor_body: Value,
}

impl CauseFacts {
    fn healthy() -> Self {
        Self {
            identity_status: 200,
            guild_status: 200,
            guild_body: healthy_guild(),
            actor_body: member_body(ACTOR_ID, &[MODERATOR_ROLE], false),
        }
    }

    fn respond(&self, method: &str, path: &str) -> RestResponse {
        let json_response = |status: u16, body: Value| RestResponse {
            status,
            body: Some(body.to_string()),
            delay: Duration::ZERO,
        };
        if method == "GET" {
            if path.ends_with("/users/@me") {
                if self.identity_status == 200 {
                    return json_response(
                        200,
                        serde_json::to_value(user(BOT_ID, true)).expect("bot identity"),
                    );
                }
                return json_response(
                    self.identity_status,
                    json!({"message": "identity unavailable", "code": 0}),
                );
            }
            // A builder refactor may read roles separately; keep that healthy so
            // only the pinned fault can refuse.
            if path.ends_with("/roles") {
                return json_response(200, self.guild_body["roles"].clone());
            }
            if path == format!("/api/v10/guilds/{GUILD_S}") {
                if self.guild_status == 200 {
                    return json_response(200, self.guild_body.clone());
                }
                let message = if self.guild_status == 403 {
                    "Missing Permissions"
                } else {
                    "Internal Server Error"
                };
                return json_response(self.guild_status, json!({"message": message, "code": 0}));
            }
            let target_body = member_body(TARGET_ID, &[], false);
            let bot_body = member_body(BOT_ID, &[BOT_ROLE], true);
            for (id, body) in [
                (ACTOR_ID, &self.actor_body),
                (TARGET_ID, &target_body),
                (BOT_ID, &bot_body),
            ] {
                if path.ends_with(&format!("/members/{id}")) {
                    return json_response(200, body.clone());
                }
            }
            return RestResponse {
                status: 500,
                body: Some(json!({"message": "unexpected authority read", "code": 0}).to_string()),
                delay: Duration::ZERO,
            };
        }
        if method == "PATCH" && path.ends_with("/messages/@original") {
            return json_response(200, json!({"id": "99"}));
        }
        // Any other mutation is a test-harness bug: fail loud, never 200.
        RestResponse {
            status: 500,
            body: Some(json!({"message": "unexpected mutation", "code": 0}).to_string()),
            delay: Duration::ZERO,
        }
    }
}

/// Route-based loopback REST double: responses follow the request route, not a
/// brittle script index, so a builder refactor that reorders reads keeps green.
struct CauseRest {
    requests: Arc<Mutex<Vec<RestRequest>>>,
    origin: String,
    task: JoinHandle<()>,
}

impl CauseRest {
    async fn start(facts: CauseFacts) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock listener");
        let origin = format!("http://{}", listener.local_addr().expect("mock address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (facts, recorded) = (facts.clone(), requests.clone());
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (facts, recorded) = (facts.clone(), recorded.clone());
                tokio::spawn(async move {
                    let Some((method, path, body)) = read_rest_request(&mut stream).await else {
                        return;
                    };
                    let response = facts.respond(&method, &path);
                    recorded.lock().expect("requests").push(RestRequest {
                        method,
                        path,
                        body,
                        received_at: std::time::Instant::now(),
                    });
                    let body = response.body.unwrap_or_default();
                    let header = format!(
                        "HTTP/1.1 {} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        response.status,
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(body.as_bytes()).await;
                });
            }
        });
        Self {
            requests,
            origin,
            task,
        }
    }

    fn requests(&self) -> Vec<RestRequest> {
        self.requests.lock().expect("recorded requests").clone()
    }
}

impl Drop for CauseRest {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn cause_database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit disposable test bootstrap");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("authorized disposable database, never a credential fallback")
}

/// Total across every `moderation_enforcements_total` series for this verb and
/// outcome, in any label order, so an added bounded cause label keeps green.
fn enforcement_total(verb: &str, outcome: &str) -> u64 {
    let text = two_bot_core::metrics::global().render(None);
    let verb_label = format!("verb=\"{verb}\"");
    let outcome_label = format!("outcome=\"{outcome}\"");
    text.lines()
        .filter(|line| {
            line.starts_with("two_bot_moderation_enforcements_total{")
                && line.contains(verb_label.as_str())
                && line.contains(outcome_label.as_str())
        })
        .filter_map(|line| line.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
}

fn completion_content(requests: &[RestRequest]) -> String {
    let replies: Vec<Value> = requests
        .iter()
        .filter(|request| {
            request.method == "PATCH" && request.path.ends_with("/messages/@original")
        })
        .map(|request| serde_json::from_slice(&request.body).expect("reply JSON"))
        .collect();
    assert_eq!(replies.len(), 1, "one bounded command completion");
    replies[0]["content"]
        .as_str()
        .expect("completion content")
        .to_owned()
}

fn guild_mutations(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests
        .iter()
        .filter(|request| {
            request.path.starts_with("/api/v10/guilds/")
                && !matches!(request.method.as_str(), "GET")
        })
        .collect()
}

/// Drive one refusal, then pin the reply, the metric, the audit and the single
/// cause line. Every assertion names its facet so a RED run shows which one.
async fn assert_refused(
    pool: &PgPool,
    rest: &CauseRest,
    interaction: &Interaction,
    action: ModerationAction,
    verb: &str,
    cause: &str,
    label: &str,
) {
    let capture = crate::tracing_capture::global();
    let start = capture.text().len();
    let before = enforcement_total(verb, "refused");
    let member = MemberRuntime::for_test(pool.clone(), GUILD_S);
    let executor =
        ActionExecutor::with_proxy("fixture-only-token".into(), Some(rest.origin.clone()))
            .expect("loopback executor");
    handle_command(&member, &executor, interaction, action).await;
    let logs = capture.text().split_off(start);

    let requests = rest.requests();
    let reply = completion_content(&requests);
    assert_eq!(reply, GENERIC_REPLY, "{label}: generic member reply");
    for token in CAUSES {
        assert!(
            !reply.contains(token),
            "{label}: reply must not leak cause {token:?}: {reply:?}"
        );
    }
    assert!(
        guild_mutations(&requests).is_empty(),
        "{label}: a refusal must not mutate: {requests:?}"
    );
    let denied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moderation_audit WHERE idempotency_key = $1 AND outcome = 'denied'",
    )
    .bind(interaction.id.get().to_string())
    .fetch_one(pool)
    .await
    .expect("refusal audit");
    assert_eq!(denied, 1, "{label}: one attributable denial audit");
    assert_eq!(
        enforcement_total(verb, "refused") - before,
        1,
        "{label}: one kept refused metric"
    );
    let lines: Vec<&str> = logs
        .lines()
        .filter(|line| line.contains(REFUSAL_LINE))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "{label}: exactly one refusal line in {logs:?}"
    );
    assert!(
        lines[0].contains(cause),
        "{label}: refusal line carries {cause:?}: {:?}",
        lines[0]
    );
}

/// Well-formed tempban/timeout vehicle: strict decode requires
/// `duration_seconds`, so the option rides along and the refusal below proves
/// the pinned fault caused it, never a malformed vehicle.
fn timed_cause_slash(verb: &str) -> Interaction {
    let mut interaction = member_slash(verb);
    let Some(InteractionData::ApplicationCommand(data)) = &mut interaction.data else {
        panic!("command fixture")
    };
    data.options
        .push(option("duration_seconds", CommandOptionValue::Integer(60)));
    interaction
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn authority_refusal_logs_discord_403_cause() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = cause_database().await;
    let mut facts = CauseFacts::healthy();
    facts.guild_status = 403;
    let rest = CauseRest::start(facts).await;
    assert_refused(
        db.pool(),
        &rest,
        &member_slash("ban"),
        ModerationAction::Ban,
        "ban",
        "discord_403",
        "discord_403",
    )
    .await;
    db.close().await.expect("dispose fixture");
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn authority_refusal_logs_transport_cause() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = cause_database().await;
    let mut facts = CauseFacts::healthy();
    facts.guild_status = 500;
    let rest = CauseRest::start(facts).await;
    assert_refused(
        db.pool(),
        &rest,
        &timed_cause_slash("tempban"),
        ModerationAction::TempBan,
        "tempban",
        "transport",
        "transport",
    )
    .await;
    db.close().await.expect("dispose fixture");
}

fn hostile_guild_payloads() -> Vec<(&'static str, Value)> {
    vec![
        (
            "roles_not_an_array",
            json!({"id": GUILD_S, "owner_id": OWNER_ID, "roles": {"101": {"position": 50}}}),
        ),
        (
            "position_not_an_integer",
            json!({
                "id": GUILD_S, "owner_id": OWNER_ID,
                "roles": [
                    {"id": GUILD_S, "position": 0, "permissions": "0"},
                    {"id": MODERATOR_ROLE, "position": "50", "permissions": "8"},
                ],
            }),
        ),
        (
            "duplicate_role_id",
            json!({
                "id": GUILD_S, "owner_id": OWNER_ID,
                "roles": [
                    {"id": GUILD_S, "position": 0, "permissions": "0"},
                    {"id": MODERATOR_ROLE, "position": 50, "permissions": "8"},
                    {"id": MODERATOR_ROLE, "position": 60, "permissions": "8"},
                ],
            }),
        ),
        (
            "missing_everyone_role",
            json!({
                "id": GUILD_S, "owner_id": OWNER_ID,
                "roles": [{"id": MODERATOR_ROLE, "position": 50, "permissions": "8"}],
            }),
        ),
        (
            "permissions_not_a_number",
            json!({
                "id": GUILD_S, "owner_id": OWNER_ID,
                "roles": [
                    {"id": GUILD_S, "position": 0, "permissions": "0"},
                    {"id": MODERATOR_ROLE, "position": 50, "permissions": "all"},
                ],
            }),
        ),
    ]
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn authority_refusal_logs_malformed_roles_cause() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    for (variant, guild_body) in hostile_guild_payloads() {
        let db = cause_database().await;
        let mut facts = CauseFacts::healthy();
        facts.guild_body = guild_body;
        let rest = CauseRest::start(facts).await;
        assert_refused(
            db.pool(),
            &rest,
            &member_slash("kick"),
            ModerationAction::Kick,
            "kick",
            "malformed_roles",
            variant,
        )
        .await;
        db.close().await.expect("dispose fixture");
    }
    // A malformed member body maps to the same token: the evidence shape is
    // wrong, not the transport.
    let db = cause_database().await;
    let mut facts = CauseFacts::healthy();
    facts.actor_body = json!({"user": {"id": "88"}, "roles": [42]});
    let rest = CauseRest::start(facts).await;
    assert_refused(
        db.pool(),
        &rest,
        &member_slash("kick"),
        ModerationAction::Kick,
        "kick",
        "malformed_roles",
        "member_roles_not_strings",
    )
    .await;
    db.close().await.expect("dispose fixture");
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn authority_refusal_logs_unknown_bot_cause() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = cause_database().await;
    let mut facts = CauseFacts::healthy();
    facts.identity_status = 500;
    let rest = CauseRest::start(facts).await;
    assert_refused(
        db.pool(),
        &rest,
        &timed_cause_slash("timeout"),
        ModerationAction::Timeout,
        "timeout",
        "unknown_bot",
        "unknown_bot",
    )
    .await;
    db.close().await.expect("dispose fixture");
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn authority_refusal_logs_guild_mismatch_cause() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = cause_database().await;
    let rest = CauseRest::start(CauseFacts::healthy()).await;
    let mut interaction = member_slash("warn");
    interaction.guild_id = Some(Id::new(OTHER_GUILD));
    assert_refused(
        db.pool(),
        &rest,
        &interaction,
        ModerationAction::Warn,
        "warn",
        "guild_mismatch",
        "guild_mismatch",
    )
    .await;
    db.close().await.expect("dispose fixture");
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn successful_warn_records_no_authority_cause_warn() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = cause_database().await;
    let rest = CauseRest::start(CauseFacts::healthy()).await;
    let capture = crate::tracing_capture::global();
    let start = capture.text().len();
    let before = enforcement_total("warn", "success");
    let member = MemberRuntime::for_test(db.pool().clone(), GUILD_S);
    let executor =
        ActionExecutor::with_proxy("fixture-only-token".into(), Some(rest.origin.clone()))
            .expect("loopback executor");
    handle_command(
        &member,
        &executor,
        &member_slash("warn"),
        ModerationAction::Warn,
    )
    .await;
    let logs = capture.text().split_off(start);

    let requests = rest.requests();
    assert_eq!(completion_content(&requests), "Warned 3333.");
    assert!(
        guild_mutations(&requests).is_empty(),
        "a warn writes the ledger, never Discord: {requests:?}"
    );
    assert_eq!(
        enforcement_total("warn", "success") - before,
        1,
        "one kept success metric"
    );
    assert!(
        !logs.lines().any(|line| line.contains(REFUSAL_LINE)),
        "success must not log a refusal line: {logs:?}"
    );
    db.close().await.expect("dispose fixture");
}
