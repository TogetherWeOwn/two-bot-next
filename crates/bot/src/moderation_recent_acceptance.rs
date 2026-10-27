//! Protected acceptance: staff-only recent applied moderation actions.
//!
//! RED-first for `/moderation-recent` (candidate name). The Builder implements
//! against this file and must never edit it; report an incorrect test to its
//! Test Author instead of working around it.
//!
//! Normative contract under test (the Builder implements exactly this):
//!
//! * One new bounded ephemeral staff-only read through the existing moderation
//!   command family, served through the shared [`CommandRuntime`] gateway path
//!   (router + defer + completion). Candidate slash name is
//!   `/moderation-recent` with one optional integer `page` (1-based, omitted
//!   == 1). The name is a proposal: the build must prove registry fit
//!   (100-command ceiling, no custom shadowing) or stop; renaming without a
//!   fresh collision proof is not permitted.
//! * Activation: existing moderation gate only (`RouterGates.moderation`,
//!   `TWO_MODERATION=1` + permitted identity). Disabled answers
//!   [`MODERATION_DISABLED_REPLY`] before any Discord or store read. Foreign
//!   guild answers the guild fence (`This command is restricted to the
//!   configured guild.`), never rows. No member/self authority is introduced.
//! * Staff authority: the existing moderation staff check, rechecked live
//!   server-side per invocation (never trust `interaction.member.permissions`
//!   alone). Required permission is `ModerateMembers` (the `/warn` gate).
//!   Honest non-staff gets the router permission refusal naming
//!   `/moderation-recent`; stale staff interaction with cold/missing/nonstaff
//!   live facts refuses after the live fetch with no rows. Missing actor
//!   permissions refuse. Never disclose rows from another guild or to
//!   non-staff.
//! * Content: guild-scoped `LIMIT`-bounded read over `moderation_audit` plus
//!   `moderation_warnings`, ordered deterministically by `created_at DESC`,
//!   `request_id ASC` (ties break by stable identity, never insertion order).
//!   Each rendered row carries its stable request identity, action, target,
//!   acting staff member, outcome word and short inert reason snippet. The
//!   reply labels these `saved ledger rows, not live-verified Discord state`
//!   (must contain `saved` + `ledger` + (`not live` or `not verified`),
//!   case-insensitive). Complete IDs only, `allowed_mentions.parse == []`,
//!   whole reply `<= 2000` UTF-16 units.
//! * Outcomes (never relabel): `applied` rows are exactly `banned`,
//!   `temporarily_banned`, `kicked`, `timed_out`, `warned`, `unbanned`,
//!   `purged`, `slowmode_updated`, `locked_down`, `unlocked`. `denied` rows
//!   are `denied`/`refused` and must render `denied` (case-insensitive) on
//!   their own line, never `applied` on that line. Everything else (`failed`,
//!   `in_progress`, `unban_failed`, unknown) renders `uncertain`
//!   (case-insensitive), never `applied`. Warn audit + warnings row sharing
//!   one `request_id` dedupes to a single display row.
//! * Pagination packs whole rows into the 2000-unit budget with footer
//!   `Page {page} of {total}` and, when more pages exist,
//!   `Next: /moderation-recent page:{next}`. Omitted page is byte-identical
//!   to page 1. Past-end and `page < 1` answer `Invalid page. Run
//!   /moderation-recent page:1, then use the next-page hint.` Empty ledger
//!   answers `No recent moderation actions in this server.` Store failure
//!   answers a private `try again` corrective (case-insensitive) with no raw
//!   error, no rows, no secrets.
//! * Strict selectors decode before any Discord or store read: duplicate,
//!   unknown or wrong-typed `page` refuses with `Cannot run
//!   /moderation-recent: ... Try: /moderation-recent ...`, naming the
//!   offending option, never echoing hostile values raw, with zero Discord
//!   guild/member reads and zero stored effects.
//! * Pure read on every path including refusals: no claim, sweep, retry,
//!   repair, delete, unban, warn, retention change, applied-mutation audit,
//!   idempotency row or live Discord mutation. Replies never carry claim
//!   tokens, nonces, raw SQL/REST errors or secrets. Existing moderation,
//!   operator and member-self-view boundaries stay intact.
//!
//! Expected results on the baseline (6 RED, 1 GREEN): every
//! `/moderation-recent` test REDs on the unknown-command reply (the command
//! is not routed yet); the existing-controls test GREENs.
//!
//! Focused check (from the task workspace, disposable DB only):
//! ```sh
//! export TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local
//! python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot --locked \
//!   moderation_recent_acceptance -- --test-threads=1
//! ```

#![cfg(test)]

use super::*;
use serde_json::{json, Value};
use two_bot_testsupport::TestDatabase;

const ACTOR: u64 = 88;
const TARGET_ONE: u64 = 3333;
const BOT: u64 = 4444;
const MOD_ROLE: &str = "101";
const BOT_ROLE: &str = "103";
const OTHER_GUILD: &str = "9999";
const COMMAND: &str = "moderation-recent";
const UNKNOWN_REPLY: &str = "I don't recognize that command.";
const EMPTY_REPLY: &str = "No recent moderation actions in this server.";
const INVALID_PAGE_REPLY: &str =
    "Invalid page. Run /moderation-recent page:1, then use the next-page hint.";

#[derive(Clone)]
struct RecentFacts {
    guild: Value,
    actor: Value,
    bot: Value,
    missing_actor: bool,
}

fn member_body(id: u64, roles: &[&str], bot: bool) -> Value {
    json!({
        "user": user(id, bot), "roles": roles, "deaf": false, "mute": false,
        "flags": 0, "pending": false, "joined_at": "2026-10-01T00:00:00+00:00",
    })
}

impl Default for RecentFacts {
    fn default() -> Self {
        let staff =
            (Permissions::BAN_MEMBERS | Permissions::KICK_MEMBERS | Permissions::MODERATE_MEMBERS)
                .bits()
                .to_string();
        Self {
            guild: json!({
                "id": GUILD_S, "owner_id": "5555",
                "roles": [
                    {"id": GUILD_S, "position": 0, "permissions": "0"},
                    {"id": MOD_ROLE, "position": 50, "permissions": staff},
                    {"id": BOT_ROLE, "position": 100, "permissions": Permissions::all().bits().to_string()},
                ],
            }),
            actor: member_body(ACTOR, &[MOD_ROLE], false),
            bot: member_body(BOT, &[BOT_ROLE], true),
            missing_actor: false,
        }
    }
}

impl RecentFacts {
    fn respond(&self, method: &str, path: &str) -> (u16, String) {
        if method == "POST" && path.ends_with("/callback") {
            return (200, r#"{"id":"99"}"#.to_owned());
        }
        if method == "PATCH" && path.ends_with("/messages/@original") {
            return (200, r#"{"id":"99"}"#.to_owned());
        }
        if method != "GET" {
            return (404, "{}".to_owned());
        }
        if path.ends_with("/users/@me") {
            let body = serde_json::to_value(user(BOT, true)).expect("bot identity");
            return (200, body.to_string());
        }
        if path == format!("/api/v10/guilds/{GUILD_S}") {
            return (200, self.guild.to_string());
        }
        if path.ends_with(&format!("/members/{ACTOR}")) {
            if self.missing_actor {
                return (
                    404,
                    r#"{"code":10007,"message":"unknown member"}"#.to_owned(),
                );
            }
            return (200, self.actor.to_string());
        }
        if path.ends_with(&format!("/members/{BOT}")) {
            return (200, self.bot.to_string());
        }
        (404, "{}".to_owned())
    }
}

struct RecentRest {
    requests: Arc<Mutex<Vec<RestRequest>>>,
    origin: String,
    task: JoinHandle<()>,
}

impl RecentRest {
    async fn start(facts: RecentFacts) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock listener");
        let origin = format!("http://{}", listener.local_addr().expect("mock address"));
        let facts = Arc::new(Mutex::new(facts));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (state, recorded) = (facts.clone(), requests.clone());
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (state, recorded) = (state.clone(), recorded.clone());
                tokio::spawn(async move {
                    let Some((method, path, body)) = read_rest_request(&mut stream).await else {
                        return;
                    };
                    let (status, text) = state.lock().expect("facts").respond(&method, &path);
                    recorded.lock().expect("requests").push(RestRequest {
                        method,
                        path,
                        body,
                        received_at: std::time::Instant::now(),
                    });
                    let reason = match status {
                        200 => "OK",
                        404 => "Not Found",
                        _ => "Internal Server Error",
                    };
                    let head = format!(
                        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        text.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(text.as_bytes()).await;
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
        self.requests.lock().expect("recorded").clone()
    }
}

impl Drop for RecentRest {
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

async fn table_count(pool: &PgPool, table: &str) -> i64 {
    assert!(
        [
            "moderation_audit",
            "moderation_warnings",
            "moderation_idempotency",
            "moderation_scheduled_unbans",
            "moderation_member_bans",
            "moderation_channel_executions",
            "moderation_lockdowns",
        ]
        .contains(&table),
        "bounded snapshot table"
    );
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
        .fetch_one(pool)
        .await
        .expect("snapshot count")
}

async fn snapshot(pool: &PgPool) -> Vec<i64> {
    let mut out = Vec::new();
    for table in [
        "moderation_audit",
        "moderation_warnings",
        "moderation_idempotency",
        "moderation_scheduled_unbans",
        "moderation_member_bans",
        "moderation_channel_executions",
        "moderation_lockdowns",
    ] {
        out.push(table_count(pool, table).await);
    }
    out
}

struct AuditSeed<'a> {
    request_id: &'a str,
    guild_id: &'a str,
    actor_id: &'a str,
    action: &'a str,
    target_id: Option<&'a str>,
    reason: &'a str,
    outcome: &'a str,
    created_at: &'a str,
}

async fn seed_audit(pool: &PgPool, seed: AuditSeed<'_>) {
    sqlx::query(
        "INSERT INTO moderation_audit
         (request_id, guild_id, actor_id, action, target_id, channel_id, reason,
          outcome, idempotency_key, metadata_json, created_at)
         VALUES ($1, $2, $3, $4, $5, NULL, $6, $7, $8, '{}', $9::text::timestamptz)",
    )
    .bind(seed.request_id)
    .bind(seed.guild_id)
    .bind(seed.actor_id)
    .bind(seed.action)
    .bind(seed.target_id)
    .bind(seed.reason)
    .bind(seed.outcome)
    .bind(format!("idem-{}", seed.request_id))
    .bind(seed.created_at)
    .execute(pool)
    .await
    .expect("seed audit ledger");
}

async fn seed_warning(
    pool: &PgPool,
    id: &str,
    guild_id: &str,
    user_id: &str,
    actor_id: &str,
    reason: &str,
    request_id: &str,
    created_at: &str,
) {
    sqlx::query(
        "INSERT INTO moderation_warnings
         (id, guild_id, user_id, actor_id, reason, request_id, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7::text::timestamptz)",
    )
    .bind(id)
    .bind(guild_id)
    .bind(user_id)
    .bind(actor_id)
    .bind(reason)
    .bind(request_id)
    .bind(created_at)
    .execute(pool)
    .await
    .expect("seed warnings ledger");
}

fn staff_interaction(options: Vec<CommandDataOption>) -> Interaction {
    let mut interaction = slash(COMMAND, Some(CHANNEL), options);
    interaction.member.as_mut().expect("actor").permissions = Some(Permissions::MODERATE_MEMBERS);
    interaction.member.as_mut().expect("actor").user = Some(user(ACTOR, false));
    interaction
}

fn page_option(page: i64) -> Vec<CommandDataOption> {
    vec![option("page", CommandOptionValue::Integer(page))]
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// Unwrap the one ephemeral completion. Fails RED with the missing command's
/// name when the router still answers unknown.
fn recent_completion(requests: &[RestRequest]) -> serde_json::Value {
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|r| r.path.ends_with("/callback"))
        .collect();
    assert_eq!(
        callbacks.len(),
        1,
        "one initial acknowledgement: {requests:?}"
    );
    assert_eq!(callbacks[0].method, "POST");
    let initial: Value = serde_json::from_slice(&callbacks[0].body).expect("callback JSON");
    if initial["type"] == 4 {
        assert_eq!(requests.len(), 1, "immediate reply is the only response");
        let data = initial["data"].clone();
        assert_eq!(data["flags"], 64, "every reply is ephemeral");
        assert_eq!(data["allowed_mentions"]["parse"], json!([]));
        let content = data["content"].as_str().unwrap_or("");
        assert!(
            !content.contains(UNKNOWN_REPLY),
            "RED: /{COMMAND} is not routed yet (unknown-command reply)"
        );
        return data;
    }
    assert_eq!(initial["type"], 5, "deferred read lifecycle");
    assert_eq!(initial["data"]["flags"], 64, "defer stays ephemeral");
    let edits: Vec<_> = requests
        .iter()
        .filter(|r| r.method == "PATCH" && r.path.ends_with("/messages/@original"))
        .collect();
    assert_eq!(edits.len(), 1, "one completion edit: {requests:?}");
    let body: Value = serde_json::from_slice(&edits[0].body).expect("completion JSON");
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    let content = body["content"].as_str().unwrap_or("");
    assert!(
        content.encode_utf16().count() <= 2000,
        "entire response fits the Discord budget"
    );
    assert!(
        !content.contains(UNKNOWN_REPLY),
        "RED: /{COMMAND} is not routed yet (unknown-command completion)"
    );
    body
}

fn recent_content(requests: &[RestRequest]) -> String {
    recent_completion(requests)["content"]
        .as_str()
        .expect("human-readable reply")
        .to_owned()
}

fn assert_no_guild_mutations(requests: &[RestRequest]) {
    for request in requests {
        if request.method == "GET" {
            continue;
        }
        let callback = request.method == "POST" && request.path.ends_with("/callback");
        let edit = request.method == "PATCH" && request.path.ends_with("/messages/@original");
        assert!(
            callback || edit,
            "reads never mutate Discord: {} {}",
            request.method,
            request.path
        );
        assert!(
            !request.path.starts_with("/api/v10/guilds/"),
            "no guild mutation: {} {}",
            request.method,
            request.path
        );
    }
    assert!(
        !requests.iter().any(|r| r.method == "DELETE"
            || r.path.contains("bulk-delete")
            || (r.method == "PUT" && r.path.contains("/bans/"))),
        "no ban/kick/delete/purge mutation"
    );
}

fn assert_no_secrets(content: &str) {
    for token in [
        "claim_token",
        "claim token",
        "nonce",
        "sqlx",
        "postgres",
        "password",
        "secret",
        "BEGIN;",
    ] {
        assert!(
            !contains_ci(content, token),
            "reply carries no internal token or raw error ({token}): {content}"
        );
    }
}

fn ledger_header(content: &str) {
    for token in ["saved", "ledger"] {
        assert!(
            contains_ci(content, token),
            "saved-ledger label names {token}: {content}"
        );
    }
    assert!(
        contains_ci(content, "not live") || contains_ci(content, "not verified"),
        "rows are not live-verified Discord state: {content}"
    );
}

fn page_footer(content: &str) -> Option<(usize, usize)> {
    let tail = content.split_once("Page ")?.1;
    let (current, total) = tail.split_once(" of ")?;
    let total: String = total.chars().take_while(char::is_ascii_digit).collect();
    Some((current.parse().ok()?, total.parse().ok()?))
}

fn line_for<'a>(content: &'a str, marker: &str) -> &'a str {
    content
        .lines()
        .find(|line| line.contains(marker))
        .unwrap_or_else(|| panic!("reply names {marker}: {content}"))
}

async fn dispatch_recent(
    pool: PgPool,
    rest: &RecentRest,
    interaction: Interaction,
) -> Vec<RestRequest> {
    let member = MemberRuntime::for_test(pool.clone(), GUILD_S);
    let runtime = CommandRuntime::new_with_member(
        pool,
        executor_at(rest.origin.clone()),
        router_with_commands(member_gates()),
        GUILD,
        true,
        member,
    );
    runtime.on_interaction(&interaction).await;
    rest.requests()
}

#[tokio::test]
async fn moderation_recent_covers_every_action_outcome_pair_exactly_once() {
    let db = database().await;
    let pool = db.pool().clone();
    // One applied row per member + channel verb, plus denied, uncertain and a
    // corroborated warning. Reverse insertion order catches accidental
    // insertion-order reads; identical outcome words across verbs catch
    // verb-blind rendering.
    let rows: Vec<(&str, &str, &str, Option<&str>, &str, &str)> = vec![
        (
            "req-unlock-001",
            "moderation.unlock",
            "unlocked",
            None,
            "reopen after raid",
            "2026-10-10T00:00:10+00:00",
        ),
        (
            "req-lockdown-001",
            "moderation.lockdown",
            "locked_down",
            None,
            "raid containment",
            "2026-10-10T00:00:09+00:00",
        ),
        (
            "req-slowmode-001",
            "moderation.slowmode",
            "slowmode_updated",
            None,
            "calm the thread",
            "2026-10-10T00:00:08+00:00",
        ),
        (
            "req-purge-001",
            "moderation.purge",
            "purged",
            None,
            "spam flood",
            "2026-10-10T00:00:07+00:00",
        ),
        (
            "req-warn-001",
            "moderation.warn",
            "warned",
            Some("3333"),
            "first warning",
            "2026-10-10T00:00:06+00:00",
        ),
        (
            "req-timeout-001",
            "moderation.timeout",
            "timed_out",
            Some("3333"),
            "cool down",
            "2026-10-10T00:00:05+00:00",
        ),
        (
            "req-kick-001",
            "moderation.kick",
            "kicked",
            Some("3334"),
            "repeated spam",
            "2026-10-10T00:00:04+00:00",
        ),
        (
            "req-tempban-001",
            "moderation.tempban",
            "temporarily_banned",
            Some("3334"),
            "raid account",
            "2026-10-10T00:00:03+00:00",
        ),
        (
            "req-ban-001",
            "moderation.ban",
            "banned",
            Some("3333"),
            "ban evasion",
            "2026-10-10T00:00:02+00:00",
        ),
        (
            "req-unban-001",
            "moderation.unban_scheduled",
            "unbanned",
            Some("3333"),
            "served time",
            "2026-10-10T00:00:01+00:00",
        ),
        (
            "req-denied-001",
            "moderation.kick",
            "denied",
            Some("3333"),
            "hierarchy refusal",
            "2026-10-10T00:00:11+00:00",
        ),
        (
            "req-failed-001",
            "moderation.ban",
            "failed",
            Some("3334"),
            "discord 5xx",
            "2026-10-10T00:00:12+00:00",
        ),
        (
            "req-progress-001",
            "moderation.timeout",
            "in_progress",
            Some("3333"),
            "uncertain dispatch",
            "2026-10-10T00:00:13+00:00",
        ),
        (
            "req-unbanfail-001",
            "moderation.unban_scheduled",
            "unban_failed",
            Some("3334"),
            "sweep failed",
            "2026-10-10T00:00:14+00:00",
        ),
    ];
    for (request, action, outcome, target, reason, at) in rows.iter().rev() {
        seed_audit(
            &pool,
            AuditSeed {
                request_id: request,
                guild_id: GUILD_S,
                actor_id: "88",
                action,
                target_id: *target,
                reason,
                outcome,
                created_at: at,
            },
        )
        .await;
    }
    seed_warning(
        &pool,
        "warn-row-1",
        GUILD_S,
        "3333",
        "88",
        "first warning",
        "req-warn-001",
        "2026-10-10T00:00:06+00:00",
    )
    .await;
    seed_audit(
        &pool,
        AuditSeed {
            request_id: "req-foreign-001",
            guild_id: OTHER_GUILD,
            actor_id: "88",
            action: "moderation.ban",
            target_id: Some("3333"),
            reason: "other guild row",
            outcome: "banned",
            created_at: "2026-10-10T00:00:20+00:00",
        },
    )
    .await;
    let before = snapshot(&pool).await;
    let rest = RecentRest::start(RecentFacts::default()).await;
    let first = dispatch_recent(pool.clone(), &rest, staff_interaction(Vec::new())).await;
    let first_again = dispatch_recent(pool.clone(), &rest, staff_interaction(page_option(1))).await;
    let after = snapshot(&pool).await;
    let requests = rest.requests();
    db.close().await.expect("drop owned fixture");
    assert_eq!(before, after, "a pure read writes no ledger row");
    assert_no_guild_mutations(&requests);
    let content = recent_content(&first);
    assert_eq!(
        recent_content(&first_again),
        content,
        "omitted page is byte-identical to page 1"
    );
    ledger_header(&content);
    assert_no_secrets(&content);
    assert!(
        !content.contains("req-foreign-001") && !content.contains(OTHER_GUILD),
        "guild-scoped read never leaks foreign rows: {content}"
    );
    // Applied rows render applied, never denied/uncertain on their line.
    for (request, action, outcome) in [
        ("req-ban-001", "moderation.ban", "banned"),
        (
            "req-tempban-001",
            "moderation.tempban",
            "temporarily_banned",
        ),
        ("req-kick-001", "moderation.kick", "kicked"),
        ("req-timeout-001", "moderation.timeout", "timed_out"),
        ("req-warn-001", "moderation.warn", "warned"),
        ("req-purge-001", "moderation.purge", "purged"),
        (
            "req-slowmode-001",
            "moderation.slowmode",
            "slowmode_updated",
        ),
        ("req-lockdown-001", "moderation.lockdown", "locked_down"),
        ("req-unlock-001", "moderation.unlock", "unlocked"),
        ("req-unban-001", "moderation.unban_scheduled", "unbanned"),
    ] {
        let line = line_for(&content, request);
        assert!(line.contains(action), "row names its action: {line}");
        assert!(
            contains_ci(line, outcome),
            "row names its applied outcome: {line}"
        );
        assert!(
            contains_ci(line, "applied"),
            "applied outcome word on its own line: {line}"
        );
        assert!(
            line.contains("88"),
            "row names the acting staff member: {line}"
        );
    }
    for target in ["3333", "3334"] {
        assert!(content.contains(target), "complete target id: {content}");
    }
    // The corroborated warning dedupes to one display row.
    assert_eq!(
        content.matches("req-warn-001").count(),
        1,
        "warn audit + warnings row share one display row: {content}"
    );
    // Denied is never relabelled as applied.
    let denied = line_for(&content, "req-denied-001");
    assert!(
        contains_ci(denied, "denied"),
        "denial keeps its word: {denied}"
    );
    assert!(
        !contains_ci(denied, "applied"),
        "a denial is never relabelled applied: {denied}"
    );
    // Uncertain outcomes render uncertain, never applied.
    for request in ["req-failed-001", "req-progress-001", "req-unbanfail-001"] {
        let line = line_for(&content, request);
        assert!(
            contains_ci(line, "uncertain"),
            "uncertain outcome word: {line}"
        );
        assert!(
            !contains_ci(line, "applied"),
            "uncertain is never relabelled applied: {line}"
        );
    }
    // Newest first: the later denied row precedes the oldest unban row.
    let denied_at = content.find("req-denied-001").expect("denied row");
    let oldest_at = content.find("req-unban-001").expect("oldest row");
    assert!(denied_at < oldest_at, "deterministic newest-first order");
}

#[tokio::test]
async fn moderation_recent_orders_same_time_ties_by_stable_identity() {
    let db = database().await;
    let pool = db.pool().clone();
    for request in ["req-tie-c", "req-tie-a", "req-tie-b"] {
        seed_audit(
            &pool,
            AuditSeed {
                request_id: request,
                guild_id: GUILD_S,
                actor_id: "88",
                action: "moderation.kick",
                target_id: Some("3333"),
                reason: "same instant",
                outcome: "kicked",
                created_at: "2026-10-11T00:00:00+00:00",
            },
        )
        .await;
    }
    let rest = RecentRest::start(RecentFacts::default()).await;
    let requests = dispatch_recent(pool.clone(), &rest, staff_interaction(Vec::new())).await;
    let snapshot_after = snapshot(&pool).await;
    db.close().await.expect("drop owned fixture");
    assert_eq!(snapshot_after[0], 3, "ordering probe reads without writing");
    let content = recent_content(&requests);
    ledger_header(&content);
    let pos = |marker: &str| content.find(marker).expect("tied row renders");
    assert!(
        pos("req-tie-a") < pos("req-tie-b") && pos("req-tie-b") < pos("req-tie-c"),
        "ties break by request identity, never insertion order: {content}"
    );
}

#[tokio::test]
async fn moderation_recent_paginates_many_rows_deterministically() {
    let db = database().await;
    let pool = db.pool().clone();
    for index in (0..30).rev() {
        let request = format!("req-page-{index:02}");
        let reason = format!("reason-{index:02} with enough text to fill the page budget");
        let created_at = format!("2026-10-12T00:00:{index:02}+00:00");
        seed_audit(
            &pool,
            AuditSeed {
                request_id: &request,
                guild_id: GUILD_S,
                actor_id: "88",
                action: "moderation.warn",
                target_id: Some("3333"),
                reason: &reason,
                outcome: "warned",
                created_at: &created_at,
            },
        )
        .await;
    }
    let before = snapshot(&pool).await;
    let rest = RecentRest::start(RecentFacts::default()).await;
    let first = dispatch_recent(pool.clone(), &rest, staff_interaction(Vec::new())).await;
    let first_content = recent_content(&first);
    let total = page_footer(&first_content)
        .map(|(_, total)| total)
        .unwrap_or(1)
        .clamp(1, 30);
    let mut pages = vec![first_content];
    for number in 2..=total {
        let requests = dispatch_recent(
            pool.clone(),
            &rest,
            staff_interaction(page_option(number as i64)),
        )
        .await;
        pages.push(recent_content(&requests));
    }
    let last_again = dispatch_recent(
        pool.clone(),
        &rest,
        staff_interaction(page_option(total as i64)),
    )
    .await;
    let past_end = dispatch_recent(
        pool.clone(),
        &rest,
        staff_interaction(page_option(total as i64 + 1)),
    )
    .await;
    let after = snapshot(&pool).await;
    db.close().await.expect("drop owned fixture");
    assert_eq!(before, after, "pagination never writes");
    assert!(total > 1, "30 rows cannot fit one 2000-unit reply");
    let mut covered = Vec::new();
    for (index, text) in pages.iter().enumerate() {
        assert_eq!(
            page_footer(text),
            Some((index + 1, total)),
            "stable page totals"
        );
        ledger_header(text);
        assert_no_secrets(text);
        let rows: Vec<_> = text.lines().filter(|l| l.contains("req-page-")).collect();
        assert!(!rows.is_empty(), "no empty intermediate page");
        covered.extend(rows.into_iter().map(str::to_owned));
        if index + 1 < total {
            assert!(
                text.contains(&format!("/moderation-recent page:{}", index + 2)),
                "actionable next page: {text}"
            );
        } else {
            assert!(
                !text.contains(&format!("/moderation-recent page:{}", total + 1)),
                "no nonexistent next page"
            );
        }
    }
    assert_eq!(covered.len(), 30, "every row exactly once across pages");
    assert_eq!(
        recent_content(&last_again),
        *pages.last().expect("last page"),
        "selection is deterministic"
    );
    assert_eq!(recent_content(&past_end), INVALID_PAGE_REPLY);
}

#[tokio::test]
async fn moderation_recent_refuses_nonstaff_missing_and_cold_actors() {
    let db = database().await;
    let pool = db.pool().clone();
    seed_audit(
        &pool,
        AuditSeed {
            request_id: "req-guarded-001",
            guild_id: GUILD_S,
            actor_id: "88",
            action: "moderation.ban",
            target_id: Some("3333"),
            reason: "guarded row",
            outcome: "banned",
            created_at: "2026-10-13T00:00:00+00:00",
        },
    )
    .await;
    let before = snapshot(&pool).await;
    // Honest non-staff: the interaction itself carries no staff bits.
    let rest = RecentRest::start(RecentFacts::default()).await;
    let mut honest = staff_interaction(Vec::new());
    honest.member.as_mut().expect("actor").permissions = Some(Permissions::empty());
    let honest_requests = dispatch_recent(pool.clone(), &rest, honest).await;
    // Stale staff interaction, live facts say otherwise: empty roles (cold),
    // missing member (404), and unprivileged roles (non-staff).
    let mut cold = RecentFacts::default();
    cold.actor = member_body(ACTOR, &[], false);
    let cold_rest = RecentRest::start(cold).await;
    let cold_requests =
        dispatch_recent(pool.clone(), &cold_rest, staff_interaction(Vec::new())).await;
    let mut missing = RecentFacts::default();
    missing.missing_actor = true;
    let missing_rest = RecentRest::start(missing).await;
    let missing_requests =
        dispatch_recent(pool.clone(), &missing_rest, staff_interaction(Vec::new())).await;
    let mut junior = RecentFacts::default();
    junior.guild = json!({
        "id": GUILD_S, "owner_id": "5555",
        "roles": [
            {"id": GUILD_S, "position": 0, "permissions": "0"},
            {"id": MOD_ROLE, "position": 50, "permissions": "0"},
            {"id": BOT_ROLE, "position": 100, "permissions": Permissions::all().bits().to_string()},
        ],
    });
    let junior_rest = RecentRest::start(junior).await;
    let junior_requests =
        dispatch_recent(pool.clone(), &junior_rest, staff_interaction(Vec::new())).await;
    let after = snapshot(&pool).await;
    // Moderation disabled and foreign guild never reach the ledger either.
    let disabled_pool = pool.clone();
    let disabled_member = MemberRuntime::for_test(disabled_pool.clone(), GUILD_S);
    let disabled_rest = RecentRest::start(RecentFacts::default()).await;
    let disabled_runtime = CommandRuntime::new_with_member(
        disabled_pool,
        executor_at(disabled_rest.origin.clone()),
        router_with_commands(RouterGates {
            moderation: false,
            ..gates(true, true)
        }),
        GUILD,
        true,
        disabled_member,
    );
    disabled_runtime
        .on_interaction(&staff_interaction(Vec::new()))
        .await;
    let disabled_requests = disabled_rest.requests();
    let mut foreign = staff_interaction(Vec::new());
    foreign.guild_id = Some(Id::new(9999));
    if let Some(InteractionData::ApplicationCommand(data)) = foreign.data.as_mut() {
        data.guild_id = Some(Id::new(9999));
    }
    let foreign_rest = RecentRest::start(RecentFacts::default()).await;
    let foreign_requests = dispatch_recent(pool.clone(), &foreign_rest, foreign).await;
    db.close().await.expect("drop owned fixture");
    assert_eq!(before, after, "refusals never write");
    for (label, requests) in [
        ("honest non-staff", honest_requests),
        ("cold actor", cold_requests),
        ("missing actor", missing_requests),
        ("unprivileged roles", junior_requests),
    ] {
        let content = recent_content(&requests);
        assert!(
            !content.contains("req-guarded-001"),
            "{label} discloses no rows: {content}"
        );
        assert!(
            contains_ci(&content, "moderate members")
                || contains_ci(&content, "permission")
                || contains_ci(&content, "not available")
                || contains_ci(&content, "failed"),
            "{label} refuses with a finite corrective: {content}"
        );
        assert_no_secrets(&content);
        assert_no_guild_mutations(&requests);
    }
    assert_eq!(
        recent_content(&disabled_requests),
        MODERATION_DISABLED_REPLY,
        "disabled moderation refuses before any read"
    );
    assert_eq!(
        recent_content(&foreign_requests),
        "This command is restricted to the configured guild.",
        "foreign guild never discloses"
    );
}

#[tokio::test]
async fn moderation_recent_rejects_malformed_selectors_before_any_read() {
    let db = database().await;
    let pool = db.pool().clone();
    seed_audit(
        &pool,
        AuditSeed {
            request_id: "req-strict-001",
            guild_id: GUILD_S,
            actor_id: "88",
            action: "moderation.kick",
            target_id: Some("3333"),
            reason: "strict probe",
            outcome: "kicked",
            created_at: "2026-10-14T00:00:00+00:00",
        },
    )
    .await;
    let before = snapshot(&pool).await;
    let invalid: Vec<Vec<CommandDataOption>> = vec![
        page_option(0),
        page_option(-1),
        page_option(i64::MIN),
        page_option(i64::MAX),
        page_option(50),
        vec![option("page", CommandOptionValue::String("2".to_owned()))],
        vec![option(
            "page",
            CommandOptionValue::String("@everyone hostile-page".to_owned()),
        )],
        vec![option("page", CommandOptionValue::Number(1.5))],
        vec![option("page", CommandOptionValue::Boolean(true))],
        vec![option("page", CommandOptionValue::SubCommand(Vec::new()))],
        vec![
            option("page", CommandOptionValue::Integer(1)),
            option("page", CommandOptionValue::Integer(2)),
        ],
        vec![option("unknown", CommandOptionValue::Integer(1))],
        vec![option("page", CommandOptionValue::User(Id::new(3333)))],
    ];
    let rest = RecentRest::start(RecentFacts::default()).await;
    let mut results = Vec::new();
    for options in invalid {
        results.push(dispatch_recent(pool.clone(), &rest, staff_interaction(options)).await);
    }
    let after = snapshot(&pool).await;
    let all_requests = rest.requests();
    db.close().await.expect("drop owned fixture");
    assert_eq!(before, after, "strict refusals never write");
    for requests in &results {
        let content = recent_content(requests);
        assert!(
            content.contains("page") || content.contains("Page"),
            "strict refusal names the selector: {content}"
        );
        assert!(
            content.contains("/moderation-recent"),
            "strict refusal names the remedy: {content}"
        );
        assert!(
            content.encode_utf16().count() <= 1000,
            "short refusal: {content}"
        );
        assert!(
            !content.contains("hostile-page"),
            "never echo hostile input: {content}"
        );
        assert!(
            !content.contains("req-strict-001"),
            "refusal serves no rows: {content}"
        );
        assert_no_secrets(&content);
        assert_no_guild_mutations(requests);
    }
    // Strict decode precedes Discord reads: only callbacks and completion
    // edits may appear, never guild/member GETs.
    for request in all_requests.iter().filter(|r| r.method == "GET") {
        assert!(
            !request.path.starts_with("/api/v10/guilds/")
                && !request.path.starts_with("/api/v10/users/"),
            "malformed selectors never reach Discord reads: {} {}",
            request.method,
            request.path
        );
    }
}

#[tokio::test]
async fn moderation_recent_empty_and_unavailable_states_stay_finite() {
    let db = database().await;
    let pool = db.pool().clone();
    let rest = RecentRest::start(RecentFacts::default()).await;
    let empty_default = dispatch_recent(pool.clone(), &rest, staff_interaction(Vec::new())).await;
    let empty_first = dispatch_recent(pool.clone(), &rest, staff_interaction(page_option(1))).await;
    let empty_second =
        dispatch_recent(pool.clone(), &rest, staff_interaction(page_option(2))).await;
    let after_empty = snapshot(&pool).await;
    db.close().await.expect("drop owned fixture");
    assert_eq!(
        after_empty,
        vec![0i64, 0, 0, 0, 0, 0, 0],
        "empty reads never write"
    );
    assert_eq!(recent_content(&empty_default), EMPTY_REPLY);
    assert_eq!(recent_content(&empty_first), EMPTY_REPLY);
    assert_eq!(recent_content(&empty_second), INVALID_PAGE_REPLY);
    // Unavailable store: an unreachable pool must still answer privately.
    let lazy = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("lazy pool");
    let failing = RecentRest::start(RecentFacts::default()).await;
    let member = MemberRuntime::for_test(lazy.clone(), GUILD_S);
    let runtime = CommandRuntime::new_with_member(
        lazy,
        executor_at(failing.origin.clone()),
        router_with_commands(member_gates()),
        GUILD,
        true,
        member,
    );
    runtime.on_interaction(&staff_interaction(Vec::new())).await;
    let failing_requests = failing.requests();
    let content = recent_content(&failing_requests);
    assert!(
        contains_ci(&content, "try again"),
        "unavailable store gets a finite retry corrective: {content}"
    );
    assert_no_secrets(&content);
    assert_no_guild_mutations(&failing_requests);
}

#[tokio::test]
async fn moderation_recent_keeps_hostile_reasons_inert_and_framed() {
    let db = database().await;
    let pool = db.pool().clone();
    let hostile =
        "@everyone @here <@3333> <#4444> <@&101> ||spoiler|| **bold** http://evil.example/x 😀"
            .repeat(4);
    seed_audit(
        &pool,
        AuditSeed {
            request_id: "req-hostile-001",
            guild_id: GUILD_S,
            actor_id: "88",
            action: "moderation.warn",
            target_id: Some("3333"),
            reason: &hostile,
            outcome: "warned",
            created_at: "2026-10-15T00:00:00+00:00",
        },
    )
    .await;
    let astral = "😀".repeat(300);
    seed_audit(
        &pool,
        AuditSeed {
            request_id: "req-astral-001",
            guild_id: GUILD_S,
            actor_id: "88",
            action: "moderation.timeout",
            target_id: Some("3334"),
            reason: &astral,
            outcome: "timed_out",
            created_at: "2026-10-15T00:00:01+00:00",
        },
    )
    .await;
    let before = snapshot(&pool).await;
    let rest = RecentRest::start(RecentFacts::default()).await;
    let requests = dispatch_recent(pool.clone(), &rest, staff_interaction(Vec::new())).await;
    let after = snapshot(&pool).await;
    db.close().await.expect("drop owned fixture");
    assert_eq!(before, after, "hostile probe never writes");
    let body = recent_completion(&requests);
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    let content = body["content"].as_str().expect("reply text");
    assert!(
        content.encode_utf16().count() <= 2000,
        "astral-heavy reply still fits the exact UTF-16 budget"
    );
    ledger_header(content);
    assert_no_secrets(content);
    // Complete IDs survive verbatim; hostile reason text renders as inert
    // content under suppressed mentions, never as a second row.
    for marker in ["req-hostile-001", "req-astral-001", "3333", "3334", "88"] {
        assert!(content.contains(marker), "complete id {marker}: {content}");
    }
    assert!(
        content.contains("@everyone"),
        "staff reason stays visible as inert text under suppressed mentions"
    );
    assert!(
        !content.contains("http://evil.example/x http://evil.example/x http://evil.example/x http://evil.example/x http://evil.example/x"),
        "unbounded reason text never explodes the frame"
    );
}

#[tokio::test]
async fn moderation_recent_existing_controls_remain_unchanged() {
    // Positive controls: these pass on the baseline and must keep passing
    // after the build. They prove the new read changes no existing surface.
    assert_eq!(
        two_bot_core::moderation::ModerationAction::Ban.command_name(),
        "ban"
    );
    assert_eq!(
        two_bot_core::moderation::ModerationAction::ALL.len(),
        9,
        "reads are not mutations"
    );
    let defs = two_bot_core::moderation::moderation_commands();
    let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
    assert!(
        names.contains(&"ban")
            && names.contains(&"warn")
            && names.contains(&"purge-preview")
            && names.contains(&"channel-inspect"),
        "existing moderation family intact: {names:?}"
    );
    let row = two_bot_core::command_permissions::command_permission("warn")
        .expect("warn keeps its permission row");
    assert_eq!(row.required_permissions, 1099511627776);
    // Router still refuses an honest non-moderator for a member verb before
    // any store work.
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("never-used pool");
    let rest = RecentRest::start(RecentFacts::default()).await;
    let member = MemberRuntime::for_test(pool.clone(), GUILD_S);
    let runtime = CommandRuntime::new_with_member(
        pool,
        executor_at(rest.origin.clone()),
        router_with_commands(member_gates()),
        GUILD,
        true,
        member,
    );
    let mut refused = slash(
        "warn",
        Some(CHANNEL),
        vec![
            option("target", CommandOptionValue::User(Id::new(TARGET_ONE))),
            option("reason", CommandOptionValue::String("control".to_owned())),
        ],
    );
    refused.member.as_mut().expect("actor").permissions = Some(Permissions::empty());
    runtime.on_interaction(&refused).await;
    let requests = rest.requests();
    assert_eq!(requests.len(), 1, "router refusal is one immediate reply");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("refusal JSON");
    let text = body["data"]["content"].as_str().unwrap_or("");
    assert!(
        text.contains("Moderate Members") && text.contains("/warn"),
        "member-verb permission copy unchanged: {text}"
    );
}
