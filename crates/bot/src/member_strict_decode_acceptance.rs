//! Protected behavioral acceptance: strictly decode member-verb options.
//!
//! RED-first for the shared `command_options` decoder in
//! `member_runtime.rs` (ban/tempban/kick/timeout/warn). The Builder implements
//! against this file and must never edit it; test corrections go to the Test
//! Author.
//!
//! Baseline bugs (the paired build owns the fix):
//! - `command_options` silently ignores wrong-typed options (`_ => {}`), so a
//!   `target` sent as String becomes `None` (confusing MissingTarget after
//!   reads), a `reason` sent as Integer becomes `""` (confusing Empty after
//!   reads), and a `duration_seconds` sent as String becomes `None`
//!   (Malformed after reads). No type correction is ever named.
//! - Duplicate options resolve last-wins with no refusal, so a second `target`
//!   silently retargets the punishment and a second `reason` silently replaces
//!   the audit text.
//! - Missing `target`/`reason`/`duration_seconds` (required per verb) and
//!   invalid values (empty/overlong reason, out-of-range duration) are refused
//!   only after membership reads, policy evaluation and a denied audit row.
//! - Unknown options (including `duration_seconds` on ban/kick/warn, which take
//!   no duration) are silently ignored; the command succeeds as if they were
//!   absent.
//! - Every refusal above consumes the actor cooldown and writes a denied audit
//!   row, so a malformed retry pollutes the ledger and blocks the next valid
//!   command.
//!
//! Builder contract (all required for GREEN):
//! 1. Decode strictly BEFORE any membership read, policy evaluation, mutation,
//!    audit write, idempotency claim or cooldown admission. Wrong types,
//!    duplicate options, missing required options, unknown options and invalid
//!    values (empty/whitespace/overlong reason, out-of-range duration) all
//!    refuse here. The required set is: `target` + `reason` for all five verbs,
//!    plus `duration_seconds` for tempban/timeout only. `duration_seconds` on
//!    ban/kick/warn is unexpected and must refuse. Missing `duration_seconds`
//!    on ban/kick/warn is the valid shape and must succeed.
//! 2. Refusal reply is one private completion (ephemeral defer + single PATCH,
//!    `allowed_mentions.parse == []`): non-empty, under 1000 chars,
//!    mention-safe (no `@everyone`/`@here`, no live mass mention via the shared
//!    `message_safety` helper), naming the offending option literally
//!    (`target`, `reason`, `duration_seconds`, or the unknown name) AND a
//!    copyable corrected invocation starting with `/<verb>` (for example
//!    `"/ban target:@user reason:text"`). Any copy satisfying these properties
//!    passes; the tests match substrings, never a full format. The reply must
//!    never echo hostile option values raw.
//! 3. Zero persistent-state effects on any strict refusal: no `moderation_audit`
//!    row, no `moderation_idempotency` row, no warning/ban/unban rows, no
//!    Discord guild/user read, no guild mutation, no cooldown consumed. A retry
//!    with the same idempotency key but corrected options must succeed with
//!    exactly one accepted audit (never a denied + accepted pair, never a key
//!    mismatch).
//! 4. Valid invocations behave byte-identically to today: same success copy
//!    (`"Banned 3333."`, `"Temporarily banned 3333."`, `"Kicked 3333."`,
//!    `"Timed out 3333."`, `"Warned 3333."`), same one mutation (warn: ledger
//!    only, no Discord mutation), same one accepted audit, same `done`
//!    idempotency, same replay (`" Already applied."`, no second mutation, no
//!    duplicate audit). Hierarchy (`"The target is equal to or above your
//!    highest role"`), cooldown (`"Wait 5 seconds before using another member
//!    moderation command."`) and policy (`"This target cannot be moderated"`,
//!    `"This moderation action requires a target"` for genuinely policy-level
//!    misses, if any remain) copies and their audit/metric behavior stay
//!    intact. The Builder pins any remaining wrong-type variants (Boolean,
//!    Number, Channel, Role, Mentionable, Attachment, Focused, SubCommand) with
//!    its own unit tests; this file pins one representative wrong type per
//!    option plus duplicates, missing, unknown and invalid values.
//!
//! Offline: none. Every test drives the real command router, member runtime,
//! REST executor and disposable ledger (loopback mock REST + fresh
//! `agent-testdb` slice per test, same as the sibling dispatch acceptance). No
//! live Discord or application database.
//!
//! Expected results on the baseline (5 RED, 1 GREEN): every strict-refusal,
//! no-cooldown and no-idempotency-pollution test REDs (reads happen, a denied
//! audit lands, the reply lacks `/<verb>`, duplicates succeed or retarget,
//! unknown options are ignored, cooldown is consumed); the valid-control test
//! GREENs (it pins today's byte-identical success shape).
//!
//! Focused check (from the task workspace, disposable DB only):
//! ```sh
//! export TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local
//! python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot --locked \
//!   member_strict_decode -- --ignored --test-threads=1
//! ```
//! The Builder and reviewers run exactly this; hierarchy/cooldown/idempotency
//! controls are inside `valid_hierarchy_cooldown_and_replay_controls` in this
//! same file, plus the existing sibling suites.

use super::*;
use serde_json::{json, Value};
use two_bot_core::message_safety::contains_mass_mention;
use two_bot_testsupport::TestDatabase;

const ACTOR: u64 = 88;
const TARGET: u64 = 3333;
const BOT: u64 = 4444;
const MODERATOR_ROLE: &str = "101";
const HIGH_ROLE: &str = "102";
const BOT_ROLE: &str = "103";
const REMOTE_MARKER: &str = "untrusted-authority-response-fixture";
const VERBS: [&str; 5] = ["ban", "tempban", "kick", "timeout", "warn"];

fn member_body(id: u64, roles: &[&str], bot: bool) -> Value {
    json!({
        "user": user(id, bot), "roles": roles, "deaf": false, "mute": false,
        "flags": 0, "pending": false, "joined_at": "2026-10-01T00:00:00Z",
    })
}

#[derive(Clone)]
struct StrictFacts {
    guild: Value,
    actor: Value,
    target: Value,
    bot: Value,
}

impl Default for StrictFacts {
    fn default() -> Self {
        let permissions =
            (Permissions::BAN_MEMBERS | Permissions::KICK_MEMBERS | Permissions::MODERATE_MEMBERS)
                .bits()
                .to_string();
        Self {
            guild: json!({
                "id": GUILD_S, "owner_id": "5555",
                "roles": [
                    {"id": GUILD_S, "position": 0, "permissions": "0"},
                    {"id": MODERATOR_ROLE, "position": 50, "permissions": permissions},
                    {"id": HIGH_ROLE, "position": 60, "permissions": "0"},
                    {"id": BOT_ROLE, "position": 100, "permissions": Permissions::all().bits().to_string()},
                ],
            }),
            actor: member_body(ACTOR, &[MODERATOR_ROLE], false),
            target: member_body(TARGET, &[], false),
            bot: member_body(BOT, &[BOT_ROLE], true),
        }
    }
}

impl StrictFacts {
    fn respond(&self, method: &str, path: &str) -> RestResponse {
        let json_response = |body: Value| RestResponse {
            status: 200,
            body: Some(body.to_string()),
            delay: Duration::ZERO,
        };
        if method == "GET" {
            if path.ends_with("/users/@me") {
                return json_response(serde_json::to_value(user(BOT, true)).expect("bot identity"));
            }
            if path == format!("/api/v10/guilds/{GUILD_S}") {
                return json_response(self.guild.clone());
            }
            if path.ends_with("/roles") {
                return json_response(self.guild["roles"].clone());
            }
            for (id, body) in [
                (ACTOR, &self.actor),
                (TARGET, &self.target),
                (BOT, &self.bot),
            ] {
                if path.ends_with(&format!("/members/{id}")) {
                    return json_response(body.clone());
                }
            }
            // Unknown members read as departed (404): ban/tempban tolerate,
            // kick/timeout/warn refuse. Never the remote marker.
            if path.contains("/members/") {
                return RestResponse {
                    status: 404,
                    body: Some(json!({"code": 10007, "message": "Unknown Member"}).to_string()),
                    delay: Duration::ZERO,
                };
            }
            return RestResponse::status(404);
        }
        if path.starts_with("/api/v10/guilds/") {
            let status = if method == "PATCH" { 200 } else { 204 };
            return RestResponse {
                status,
                body: Some(self.target.to_string()),
                delay: Duration::ZERO,
            };
        }
        json_response(json!({"id": "99"}))
    }
}

struct StrictRest {
    requests: Arc<Mutex<Vec<RestRequest>>>,
    origin: String,
    task: JoinHandle<()>,
}

impl StrictRest {
    async fn start(facts: StrictFacts) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock listener");
        let origin = format!("http://{}", listener.local_addr().expect("mock address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (facts_clone, recorded) = (facts, requests.clone());
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (facts, recorded) = (facts_clone.clone(), recorded.clone());
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

impl Drop for StrictRest {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit disposable test bootstrap");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("authorized disposable database, never a credential fallback")
}

fn runtime(pool: &PgPool, rest: &StrictRest) -> Arc<CommandRuntime> {
    let member = MemberRuntime::for_test(pool.clone(), GUILD_S);
    let executor =
        ActionExecutor::with_proxy("fixture-only-token".into(), Some(rest.origin.clone()))
            .expect("loopback executor");
    CommandRuntime::new_with_member(
        pool.clone(),
        executor,
        router_with_commands(member_gates()),
        GUILD,
        true,
        member,
    )
}

fn strict_interaction(verb: &str, key: u64, options: Vec<CommandDataOption>) -> Interaction {
    let mut interaction = slash(verb, Some(CHANNEL), options);
    interaction.id = Id::new(key);
    interaction.token = format!("strict-test-{key}");
    interaction.member.as_mut().expect("actor").permissions = Some(Permissions::all());
    interaction
}

fn valid_options(verb: &str) -> Vec<CommandDataOption> {
    let mut options = vec![
        option("target", CommandOptionValue::User(Id::new(TARGET))),
        option("reason", CommandOptionValue::String("wiring".to_owned())),
    ];
    if matches!(verb, "tempban" | "timeout") {
        options.push(option("duration_seconds", CommandOptionValue::Integer(60)));
    }
    options
}

fn guild_reads(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests
        .iter()
        .filter(|r| {
            r.method == "GET"
                && (r.path.starts_with("/api/v10/guilds/")
                    || r.path.ends_with("/users/@me")
                    || r.path.ends_with("/roles"))
        })
        .collect()
}

fn guild_mutations(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests
        .iter()
        .filter(|r| r.method != "GET" && r.path.starts_with("/api/v10/guilds/"))
        .collect()
}

fn completion_content(requests: &[RestRequest]) -> String {
    let replies: Vec<Value> = requests
        .iter()
        .filter(|r| r.method == "PATCH" && r.path.ends_with("/messages/@original"))
        .map(|r| serde_json::from_slice(&r.body).expect("reply JSON"))
        .collect();
    assert_eq!(replies.len(), 1, "one bounded command completion");
    assert_eq!(replies[0]["allowed_mentions"]["parse"], json!([]));
    replies[0]["content"]
        .as_str()
        .expect("completion content")
        .to_owned()
}

fn assert_ephemeral_defer(requests: &[RestRequest], label: &str) {
    let defers: Vec<Value> = requests
        .iter()
        .filter(|r| r.path.ends_with("/callback"))
        .map(|r| serde_json::from_slice(&r.body).expect("defer JSON"))
        .collect();
    assert_eq!(defers.len(), 1, "{label}: one defer");
    assert_eq!(defers[0]["type"], 5, "{label}: deferred reply");
    assert_eq!(defers[0]["data"]["flags"], 64, "{label}: private defer");
}

async fn audit_rows(pool: &PgPool, key: u64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM moderation_audit WHERE idempotency_key = $1")
        .bind(key.to_string())
        .fetch_one(pool)
        .await
        .expect("audit evidence")
}

async fn idempotency_rows(pool: &PgPool, key: u64) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM moderation_idempotency WHERE guild_id = $1 AND idempotency_key = $2",
    )
    .bind(GUILD_S)
    .bind(key.to_string())
    .fetch_one(pool)
    .await
    .expect("idempotency evidence")
}

async fn ledger_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT (SELECT count(*) FROM moderation_warnings) + (SELECT count(*) FROM moderation_member_bans) + (SELECT count(*) FROM moderation_scheduled_unbans)")
        .fetch_one(pool)
        .await
        .expect("zero-effect ledger evidence")
}

/// Pin the strict refusal: no reads, no mutations, no stored rows, one private
/// completion naming the offending option with a `/verb` correction.
async fn assert_strict_refusal(
    pool: &PgPool,
    rest: &StrictRest,
    key: u64,
    verb: &str,
    offending: &str,
    label: &str,
) {
    let requests = rest.requests();
    assert!(
        guild_reads(&requests).is_empty(),
        "{label}/{verb}: strict decode precedes every membership read: {:?}",
        requests
            .iter()
            .map(|r| (&r.method, &r.path))
            .collect::<Vec<_>>()
    );
    assert!(
        guild_mutations(&requests).is_empty(),
        "{label}/{verb}: strict refusal never mutates: {requests:?}"
    );
    assert_ephemeral_defer(&requests, &format!("{label}/{verb}"));
    let reply = completion_content(&requests);
    assert!(
        !reply.is_empty() && reply.len() < 1000,
        "{label}/{verb}: bounded private refusal, got {reply:?}"
    );
    assert!(
        reply.contains(offending),
        "{label}/{verb}: refusal names the offending option {offending:?}, got {reply:?}"
    );
    assert!(
        reply.contains(&format!("/{verb}")),
        "{label}/{verb}: refusal carries a copyable /{verb} correction, got {reply:?}"
    );
    assert!(
        !reply.contains(REMOTE_MARKER),
        "{label}/{verb}: remote errors are never echoed"
    );
    assert!(
        !reply.contains("@everyone") && !reply.contains("@here"),
        "{label}/{verb}: refusal is mention-safe, got {reply:?}"
    );
    assert!(
        !contains_mass_mention(&reply),
        "{label}/{verb}: refusal carries no live mention, got {reply:?}"
    );
    assert_eq!(
        audit_rows(pool, key).await,
        0,
        "{label}/{verb}: no audit row on a strict refusal"
    );
    assert_eq!(
        idempotency_rows(pool, key).await,
        0,
        "{label}/{verb}: no idempotency claim on a strict refusal"
    );
    assert_eq!(
        ledger_rows(pool).await,
        0,
        "{label}/{verb}: no warning, prepared intent or expiry"
    );
}

fn expected_success(verb: &str) -> &'static str {
    match verb {
        "ban" => "Banned 3333.",
        "tempban" => "Temporarily banned 3333.",
        "kick" => "Kicked 3333.",
        "timeout" => "Timed out 3333.",
        "warn" => "Warned 3333.",
        _ => unreachable!(),
    }
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn wrong_typed_options_refuse_before_any_read_or_mutation() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = database().await;
    // (case label, offending option, options builder)
    let cases: Vec<(&str, &str)> = vec![
        ("target as string", "target"),
        ("reason as integer", "reason"),
    ];
    let mut key = 100_000u64;
    for verb in VERBS {
        for &(label, offending) in &cases {
            let options = match label {
                "target as string" => {
                    let mut opts = vec![option(
                        "target",
                        CommandOptionValue::String(TARGET.to_string()),
                    )];
                    opts.push(option(
                        "reason",
                        CommandOptionValue::String("wiring".to_owned()),
                    ));
                    if matches!(verb, "tempban" | "timeout") {
                        opts.push(option("duration_seconds", CommandOptionValue::Integer(60)));
                    }
                    opts
                }
                "reason as integer" => {
                    let mut opts =
                        vec![option("target", CommandOptionValue::User(Id::new(TARGET)))];
                    opts.push(option("reason", CommandOptionValue::Integer(123)));
                    if matches!(verb, "tempban" | "timeout") {
                        opts.push(option("duration_seconds", CommandOptionValue::Integer(60)));
                    }
                    opts
                }
                _ => unreachable!(),
            };
            let rest = StrictRest::start(StrictFacts::default()).await;
            let rt = runtime(db.pool(), &rest);
            rt.on_interaction(&strict_interaction(verb, key, options))
                .await;
            assert_strict_refusal(db.pool(), &rest, key, verb, offending, label).await;
            key += 1;
        }
        if matches!(verb, "tempban" | "timeout") {
            let options = vec![
                option("target", CommandOptionValue::User(Id::new(TARGET))),
                option("reason", CommandOptionValue::String("wiring".to_owned())),
                option(
                    "duration_seconds",
                    CommandOptionValue::String("60".to_owned()),
                ),
            ];
            let rest = StrictRest::start(StrictFacts::default()).await;
            let rt = runtime(db.pool(), &rest);
            rt.on_interaction(&strict_interaction(verb, key, options))
                .await;
            assert_strict_refusal(
                db.pool(),
                &rest,
                key,
                verb,
                "duration_seconds",
                "duration as string",
            )
            .await;
            key += 1;
        }
    }
    db.close().await.expect("dispose fixture");
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn duplicated_options_refuse_before_any_read_or_mutation() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = database().await;
    let mut key = 110_000u64;
    for verb in VERBS {
        // Duplicate target (same value twice): baseline last-wins succeeds.
        let mut dup_target = vec![
            option("target", CommandOptionValue::User(Id::new(TARGET))),
            option("target", CommandOptionValue::User(Id::new(TARGET))),
            option("reason", CommandOptionValue::String("wiring".to_owned())),
        ];
        if matches!(verb, "tempban" | "timeout") {
            dup_target.push(option("duration_seconds", CommandOptionValue::Integer(60)));
        }
        let rest = StrictRest::start(StrictFacts::default()).await;
        let rt = runtime(db.pool(), &rest);
        rt.on_interaction(&strict_interaction(verb, key, dup_target))
            .await;
        assert_strict_refusal(db.pool(), &rest, key, verb, "target", "duplicate target").await;
        key += 1;

        // Duplicate reason: baseline last-wins keeps the second text.
        let mut dup_reason = vec![
            option("target", CommandOptionValue::User(Id::new(TARGET))),
            option("reason", CommandOptionValue::String("wiring".to_owned())),
            option("reason", CommandOptionValue::String("second".to_owned())),
        ];
        if matches!(verb, "tempban" | "timeout") {
            dup_reason.push(option("duration_seconds", CommandOptionValue::Integer(60)));
        }
        let rest = StrictRest::start(StrictFacts::default()).await;
        let rt = runtime(db.pool(), &rest);
        rt.on_interaction(&strict_interaction(verb, key, dup_reason))
            .await;
        assert_strict_refusal(db.pool(), &rest, key, verb, "reason", "duplicate reason").await;
        key += 1;

        if matches!(verb, "tempban" | "timeout") {
            let dup_duration = vec![
                option("target", CommandOptionValue::User(Id::new(TARGET))),
                option("reason", CommandOptionValue::String("wiring".to_owned())),
                option("duration_seconds", CommandOptionValue::Integer(60)),
                option("duration_seconds", CommandOptionValue::Integer(120)),
            ];
            let rest = StrictRest::start(StrictFacts::default()).await;
            let rt = runtime(db.pool(), &rest);
            rt.on_interaction(&strict_interaction(verb, key, dup_duration))
                .await;
            assert_strict_refusal(
                db.pool(),
                &rest,
                key,
                verb,
                "duration_seconds",
                "duplicate duration",
            )
            .await;
            key += 1;
        }
    }
    // Duplicate target with different values retargets on the baseline: the
    // second id wins and the wrong member is evaluated. Strict decode refuses.
    let retarget = vec![
        option("target", CommandOptionValue::User(Id::new(TARGET))),
        option("target", CommandOptionValue::User(Id::new(9999))),
        option("reason", CommandOptionValue::String("wiring".to_owned())),
    ];
    let rest = StrictRest::start(StrictFacts::default()).await;
    let rt = runtime(db.pool(), &rest);
    rt.on_interaction(&strict_interaction("ban", key, retarget))
        .await;
    assert_strict_refusal(
        db.pool(),
        &rest,
        key,
        "ban",
        "target",
        "duplicate target retargets",
    )
    .await;
    // Hostile duplicate reason: baseline persists the second text; strict
    // decode refuses without echoing it.
    let hostile = vec![
        option("target", CommandOptionValue::User(Id::new(TARGET))),
        option("reason", CommandOptionValue::String("wiring".to_owned())),
        option(
            "reason",
            CommandOptionValue::String("@everyone <@999> hostile".to_owned()),
        ),
    ];
    let rest = StrictRest::start(StrictFacts::default()).await;
    let rt = runtime(db.pool(), &rest);
    key += 1;
    rt.on_interaction(&strict_interaction("warn", key, hostile))
        .await;
    assert_strict_refusal(
        db.pool(),
        &rest,
        key,
        "warn",
        "reason",
        "duplicate hostile reason",
    )
    .await;
    db.close().await.expect("dispose fixture");
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn missing_required_and_unknown_options_refuse_while_missing_optional_succeeds() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = database().await;
    let mut key = 120_000u64;
    for verb in VERBS {
        // Missing target (required for all five).
        let mut no_target = vec![option(
            "reason",
            CommandOptionValue::String("wiring".to_owned()),
        )];
        if matches!(verb, "tempban" | "timeout") {
            no_target.push(option("duration_seconds", CommandOptionValue::Integer(60)));
        }
        let rest = StrictRest::start(StrictFacts::default()).await;
        let rt = runtime(db.pool(), &rest);
        rt.on_interaction(&strict_interaction(verb, key, no_target))
            .await;
        assert_strict_refusal(db.pool(), &rest, key, verb, "target", "missing target").await;
        key += 1;

        // Missing reason (required for all five).
        let mut no_reason = vec![option("target", CommandOptionValue::User(Id::new(TARGET)))];
        if matches!(verb, "tempban" | "timeout") {
            no_reason.push(option("duration_seconds", CommandOptionValue::Integer(60)));
        }
        let rest = StrictRest::start(StrictFacts::default()).await;
        let rt = runtime(db.pool(), &rest);
        rt.on_interaction(&strict_interaction(verb, key, no_reason))
            .await;
        assert_strict_refusal(db.pool(), &rest, key, verb, "reason", "missing reason").await;
        key += 1;

        // Unknown option alongside a valid invocation: baseline ignores it.
        let mut unknown = valid_options(verb);
        unknown.push(option("count", CommandOptionValue::Integer(5)));
        let rest = StrictRest::start(StrictFacts::default()).await;
        let rt = runtime(db.pool(), &rest);
        rt.on_interaction(&strict_interaction(verb, key, unknown))
            .await;
        assert_strict_refusal(db.pool(), &rest, key, verb, "count", "unknown option").await;
        key += 1;

        if matches!(verb, "tempban" | "timeout") {
            // Missing duration (required for tempban/timeout).
            let no_duration = vec![
                option("target", CommandOptionValue::User(Id::new(TARGET))),
                option("reason", CommandOptionValue::String("wiring".to_owned())),
            ];
            let rest = StrictRest::start(StrictFacts::default()).await;
            let rt = runtime(db.pool(), &rest);
            rt.on_interaction(&strict_interaction(verb, key, no_duration))
                .await;
            assert_strict_refusal(
                db.pool(),
                &rest,
                key,
                verb,
                "duration_seconds",
                "missing duration",
            )
            .await;
            key += 1;
        } else {
            // Unexpected duration on ban/kick/warn: baseline ignores it and
            // succeeds; strict decode refuses it as an unexpected option.
            let mut unexpected = valid_options(verb);
            unexpected.push(option("duration_seconds", CommandOptionValue::Integer(60)));
            let rest = StrictRest::start(StrictFacts::default()).await;
            let rt = runtime(db.pool(), &rest);
            rt.on_interaction(&strict_interaction(verb, key, unexpected))
                .await;
            assert_strict_refusal(
                db.pool(),
                &rest,
                key,
                verb,
                "duration_seconds",
                "unexpected duration",
            )
            .await;
            key += 1;
        }
    }
    // Missing-optional control: ban/kick/warn without duration succeed with
    // today's byte-identical copy. These stay GREEN on the baseline and after.
    for verb in ["ban", "kick", "warn"] {
        let rest = StrictRest::start(StrictFacts::default()).await;
        let rt = runtime(db.pool(), &rest);
        rt.on_interaction(&strict_interaction(verb, key, valid_options(verb)))
            .await;
        let requests = rest.requests();
        assert_ephemeral_defer(&requests, &format!("missing-optional/{verb}"));
        assert_eq!(
            completion_content(&requests),
            expected_success(verb),
            "missing-optional/{verb}: valid shape stays byte-identical"
        );
        key += 1;
    }
    db.close().await.expect("dispose fixture");
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn invalid_option_values_refuse_before_any_read_or_mutation() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = database().await;
    let mut key = 130_000u64;
    for verb in VERBS {
        for (label, reason) in [
            ("empty reason", String::new()),
            ("whitespace reason", "   ".to_owned()),
            ("overlong reason", "x".repeat(513)),
        ] {
            let mut options = vec![option("target", CommandOptionValue::User(Id::new(TARGET)))];
            options.push(option("reason", CommandOptionValue::String(reason)));
            if matches!(verb, "tempban" | "timeout") {
                options.push(option("duration_seconds", CommandOptionValue::Integer(60)));
            }
            let rest = StrictRest::start(StrictFacts::default()).await;
            let rt = runtime(db.pool(), &rest);
            rt.on_interaction(&strict_interaction(verb, key, options))
                .await;
            assert_strict_refusal(db.pool(), &rest, key, verb, "reason", label).await;
            key += 1;
        }
        if matches!(verb, "tempban" | "timeout") {
            let max = if verb == "tempban" {
                31_536_000
            } else {
                2_419_200
            };
            for (label, duration) in [
                ("duration below minimum", 59),
                ("duration zero", 0),
                ("duration above maximum", max + 1),
            ] {
                let options = vec![
                    option("target", CommandOptionValue::User(Id::new(TARGET))),
                    option("reason", CommandOptionValue::String("wiring".to_owned())),
                    option("duration_seconds", CommandOptionValue::Integer(duration)),
                ];
                let rest = StrictRest::start(StrictFacts::default()).await;
                let rt = runtime(db.pool(), &rest);
                rt.on_interaction(&strict_interaction(verb, key, options))
                    .await;
                assert_strict_refusal(db.pool(), &rest, key, verb, "duration_seconds", label).await;
                key += 1;
            }
        }
    }
    db.close().await.expect("dispose fixture");
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn strict_refusal_consumes_no_cooldown_or_idempotency() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = database().await;
    // Cooldown: a malformed command must not block the next valid one from the
    // same actor. Baseline consumes the 5 s actor cooldown before decoding.
    let mut key = 140_000u64;
    for verb in VERBS {
        let rest = StrictRest::start(StrictFacts::default()).await;
        let rt = runtime(db.pool(), &rest);
        let malformed = vec![
            option("target", CommandOptionValue::String(TARGET.to_string())),
            option("reason", CommandOptionValue::String("wiring".to_owned())),
        ];
        let malformed = if matches!(verb, "tempban" | "timeout") {
            let mut opts = malformed;
            opts.push(option("duration_seconds", CommandOptionValue::Integer(60)));
            opts
        } else {
            malformed
        };
        rt.on_interaction(&strict_interaction(verb, key, malformed))
            .await;
        // The strict refusal itself carries the correction; the cooldown proof
        // is that the immediate valid retry succeeds instead of cooldown.
        key += 1;
        rt.on_interaction(&strict_interaction(verb, key, valid_options(verb)))
            .await;
        let requests = rest.requests();
        let completions: Vec<Value> = requests
            .iter()
            .filter(|r| r.method == "PATCH" && r.path.ends_with("/messages/@original"))
            .map(|r| serde_json::from_slice(&r.body).expect("reply JSON"))
            .collect();
        assert_eq!(
            completions.len(),
            2,
            "{verb}: malformed plus valid retry complete"
        );
        let first = completions[0]["content"].as_str().expect("refusal content");
        assert!(
            first.contains("target") && first.contains(&format!("/{verb}")),
            "{verb}: first reply is the strict correction, got {first:?}"
        );
        let second = completions[1]["content"].as_str().expect("retry content");
        assert_eq!(
            second,
            expected_success(verb),
            "{verb}: valid retry after a strict refusal is not cooldown-blocked, got {second:?}"
        );
        key += 1;
    }
    // Idempotency: same key, first malformed then corrected. Baseline writes a
    // denied audit for the first, so the key ends with two rows; strict decode
    // writes nothing for the first, so exactly one accepted audit remains.
    let idem_key = 150_000u64;
    let rest = StrictRest::start(StrictFacts::default()).await;
    let rt = runtime(db.pool(), &rest);
    rt.on_interaction(&strict_interaction(
        "ban",
        idem_key,
        vec![option(
            "reason",
            CommandOptionValue::String("wiring".to_owned()),
        )],
    ))
    .await;
    rt.on_interaction(&strict_interaction("ban", idem_key, valid_options("ban")))
        .await;
    let audits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM moderation_audit WHERE idempotency_key = $1")
            .bind(idem_key.to_string())
            .fetch_one(db.pool())
            .await
            .expect("idempotency audit evidence");
    assert_eq!(
        audits, 1,
        "a corrected retry leaves exactly one accepted audit, never a denied pair"
    );
    let accepted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moderation_audit WHERE idempotency_key = $1 AND outcome <> 'denied'",
    )
    .bind(idem_key.to_string())
    .fetch_one(db.pool())
    .await
    .expect("accepted audit evidence");
    assert_eq!(accepted, 1, "the surviving audit is the accepted one");
    db.close().await.expect("dispose fixture");
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn valid_hierarchy_cooldown_and_replay_controls_stay_byte_identical() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = database().await;
    let mut key = 160_000u64;
    // Valid controls for all five verbs: byte-identical success shape.
    for verb in VERBS {
        let rest = StrictRest::start(StrictFacts::default()).await;
        let rt = runtime(db.pool(), &rest);
        let interaction = strict_interaction(verb, key, valid_options(verb));
        rt.on_interaction(&interaction).await;
        let requests = rest.requests();
        assert_ephemeral_defer(&requests, &format!("valid/{verb}"));
        assert_eq!(
            completion_content(&requests),
            expected_success(verb),
            "valid/{verb}: success copy stays byte-identical"
        );
        assert_eq!(
            guild_mutations(&requests).len(),
            usize::from(verb != "warn"),
            "valid/{verb}: effect dispatch unchanged"
        );
        let accepted: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moderation_audit WHERE idempotency_key = $1 AND outcome <> 'denied'",
        )
        .bind(key.to_string())
        .fetch_one(db.pool())
        .await
        .expect("accepted audit");
        assert_eq!(accepted, 1, "valid/{verb}: one accepted audit");
        let state: Option<String> = sqlx::query_scalar(
            "SELECT state FROM moderation_idempotency WHERE guild_id = $1 AND idempotency_key = $2",
        )
        .bind(GUILD_S)
        .bind(key.to_string())
        .fetch_optional(db.pool())
        .await
        .expect("idempotency state");
        assert_eq!(state.as_deref(), Some("done"), "valid/{verb}: done claim");
        // Replay the same interaction: no second mutation, replay marker last.
        rt.on_interaction(&interaction).await;
        let replayed = rest.requests();
        assert_eq!(
            guild_mutations(&replayed).len(),
            usize::from(verb != "warn"),
            "valid/{verb}: replay never mutates twice"
        );
        let replies: Vec<Value> = replayed
            .iter()
            .filter(|r| r.method == "PATCH" && r.path.ends_with("/messages/@original"))
            .map(|r| serde_json::from_slice(&r.body).expect("reply JSON"))
            .collect();
        assert_eq!(replies.len(), 2, "valid/{verb}: two completions");
        let second = replies[1]["content"].as_str().expect("replay content");
        assert!(
            second.ends_with("Already applied."),
            "valid/{verb}: replay keeps the marker last, got {second:?}"
        );
        key += 1;
    }
    // Hierarchy control: a higher target still denies with an audit, after
    // reads. Strict decode must not swallow policy refusals.
    let mut promoted = StrictFacts::default();
    promoted.target["roles"] = json!([HIGH_ROLE]);
    let rest = StrictRest::start(promoted).await;
    let rt = runtime(db.pool(), &rest);
    rt.on_interaction(&strict_interaction("ban", key, valid_options("ban")))
        .await;
    let requests = rest.requests();
    assert!(
        !guild_reads(&requests).is_empty(),
        "hierarchy control still reads current facts"
    );
    assert!(
        guild_mutations(&requests).is_empty(),
        "hierarchy control never mutates"
    );
    assert_eq!(
        completion_content(&requests),
        "The target is equal to or above your highest role",
        "hierarchy copy stays byte-identical"
    );
    let denied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moderation_audit WHERE idempotency_key = $1 AND outcome = 'denied'",
    )
    .bind(key.to_string())
    .fetch_one(db.pool())
    .await
    .expect("denied audit");
    assert_eq!(denied, 1, "hierarchy control keeps its denial audit");
    key += 1;
    // Cooldown control: two rapid valid commands, the second cooldowns. Strict
    // decode preserves the 5 s actor cooldown for valid work.
    let rest = StrictRest::start(StrictFacts::default()).await;
    let rt = runtime(db.pool(), &rest);
    rt.on_interaction(&strict_interaction("warn", key, valid_options("warn")))
        .await;
    key += 1;
    rt.on_interaction(&strict_interaction("warn", key, valid_options("warn")))
        .await;
    let cooldowned = rest.requests();
    let replies: Vec<Value> = cooldowned
        .iter()
        .filter(|r| r.method == "PATCH" && r.path.ends_with("/messages/@original"))
        .map(|r| serde_json::from_slice(&r.body).expect("reply JSON"))
        .collect();
    assert_eq!(replies.len(), 2, "cooldown control completes twice");
    assert_eq!(
        replies[1]["content"].as_str().expect("cooldown content"),
        "Wait 5 seconds before using another member moderation command.",
        "cooldown copy stays byte-identical"
    );
    db.close().await.expect("dispose fixture");
}
