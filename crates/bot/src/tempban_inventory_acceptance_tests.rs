//! Protected acceptance: inspect saved temporary-ban expiries, never execute them.
//! Catches stale authority, uncertainty-only listings, unstable/unbounded pages,
//! unsafe output, false empty/completed claims, and hidden ledger/Discord writes.
//! Public selector contract: /tempban-list [page:1..10000], ten complete rows/page.

#![cfg(test)]

use super::*;
use crate::hotpath_settings_test_support::queries::{Queries, Query};
use serde_json::{json, Value};
use two_bot_testsupport::TestDatabase;

const COMMAND: &str = "tempban-list";
const ACTOR: u64 = 88;
const BOT: u64 = 4444;
const FOREIGN: &str = "9999";
const PRIVATE: &str = "private-reason-🦀-@everyone-<@123>-```";
const TOKEN: &str = "fixture-claim-capability-never-display";

#[derive(Clone, Copy, Default)]
enum Authority {
    #[default]
    Staff,
    Ordinary,
    Missing,
    Cold,
}

struct Rest {
    origin: String,
    requests: Arc<Mutex<Vec<RestRequest>>>,
    task: JoinHandle<()>,
}

impl Rest {
    async fn start(authority: Authority) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture listen");
        let origin = format!("http://{}", listener.local_addr().expect("fixture address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let Some((method, path, body)) = read_rest_request(&mut stream).await else {
                        return;
                    };
                    recorded.lock().expect("requests").push(RestRequest {
                        method: method.clone(),
                        path: path.clone(),
                        body,
                        received_at: std::time::Instant::now(),
                    });
                    let (status, response) = authority_response(authority, &method, &path);
                    let body = response.to_string();
                    let header = format!(
                        "HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()
                    );
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(body.as_bytes()).await;
                });
            }
        });
        Self {
            origin,
            requests,
            task,
        }
    }

    fn requests(&self) -> Vec<RestRequest> {
        self.requests.lock().expect("requests").clone()
    }
}

impl Drop for Rest {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn authority_response(authority: Authority, method: &str, path: &str) -> (u16, Value) {
    if method != "GET" {
        return (200, json!({"id":"99"}));
    }
    if matches!(authority, Authority::Cold) {
        return (503, json!({"message": PRIVATE}));
    }
    if path.ends_with("/users/@me") {
        return (200, serde_json::to_value(user(BOT, true)).expect("bot"));
    }
    let roles = json!([
        {"id":GUILD_S,"position":0,"permissions":"0"},
        {"id":"101","position":50,"permissions":"4"},
        {"id":"103","position":100,"permissions":Permissions::all().bits().to_string()}
    ]);
    if path == format!("/api/v10/guilds/{GUILD_S}") {
        return (200, json!({"id":GUILD_S,"owner_id":"5555","roles":roles}));
    }
    if path.ends_with("/roles") {
        return (200, roles);
    }
    if path.ends_with(&format!("/members/{ACTOR}")) {
        if matches!(authority, Authority::Missing) {
            return (404, json!({"message": PRIVATE,"code":10007}));
        }
        let roles = if matches!(authority, Authority::Staff) {
            vec!["101"]
        } else {
            vec![]
        };
        return (
            200,
            json!({"user":user(ACTOR,false),"roles":roles,"deaf":false,"mute":false,"flags":0,"pending":false,"joined_at":"2026-10-01T00:00:00+00:00"}),
        );
    }
    if path.ends_with(&format!("/members/{BOT}")) {
        return (
            200,
            json!({"user":user(BOT,true),"roles":["103"],"deaf":false,"mute":false,"flags":0,"pending":false,"joined_at":"2026-10-01T00:00:00+00:00"}),
        );
    }
    (404, json!({"message":PRIVATE}))
}

async fn database() -> TestDatabase {
    let url =
        std::env::var("TWO_TEST_DATABASE_URL").expect("authorized disposable bootstrap required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded owned test database, never staging/production")
}

async fn seed(
    pool: &PgPool,
    guild: &str,
    request: &str,
    member: &str,
    state: &str,
    due: &str,
    uncertain: bool,
) {
    sqlx::query("INSERT INTO moderation_member_bans (request_id,guild_id,user_id,state,created_at) VALUES ($1,$2,$3,$4,'2026-10-01T00:00:00+00:00')")
        .bind(request).bind(guild).bind(member)
        .bind(if state == "staged" { "prepared" } else { "accepted" })
        .execute(pool).await.expect("seed ban ownership");
    sqlx::query("INSERT INTO moderation_scheduled_unbans (request_id,guild_id,user_id,execute_at,reason,state,created_at,claimed_at,claim_token,dispatch_uncertain,retry_generation) VALUES ($1,$2,$3,$4::text::timestamptz,$5,$6,'2026-10-01T00:00:00+00:00','2026-10-01T00:01:00+00:00',$7,$8,1)")
        .bind(request).bind(guild).bind(member).bind(due).bind(PRIVATE).bind(state).bind(TOKEN).bind(uncertain)
        .execute(pool).await.expect("seed saved expiry, with non-display secrets");
}

async fn snapshot(pool: &PgPool) -> Vec<String> {
    let mut result = Vec::new();
    for table in [
        "moderation_scheduled_unbans",
        "moderation_member_bans",
        "moderation_idempotency",
        "moderation_audit",
        "moderation_warnings",
    ] {
        let query = format!("SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text),'[]'::jsonb)::text FROM {table} t");
        result.push(
            // Only the fixed table identifiers above enter this statement.
            sqlx::query_scalar(sqlx::AssertSqlSafe(query.as_str()))
                .fetch_one(pool)
                .await
                .expect("full ledger snapshot"),
        );
    }
    result
}

async fn install_write_probe(pool: &PgPool) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(
        "CREATE SEQUENCE inventory_write_probe;
        CREATE FUNCTION inventory_write_seen() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN PERFORM nextval('inventory_write_probe'); RETURN NULL; END $$;",
    ))
    .execute(pool)
    .await
    .expect("nontransactional write observer");
    for table in [
        "moderation_scheduled_unbans",
        "moderation_member_bans",
        "moderation_idempotency",
        "moderation_audit",
        "moderation_warnings",
    ] {
        let ddl = format!("CREATE TRIGGER inventory_no_writes BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE ON {table} FOR EACH STATEMENT EXECUTE FUNCTION inventory_write_seen()");
        // Only the fixed table identifiers above enter this fixture DDL.
        sqlx::query(sqlx::AssertSqlSafe(ddl.as_str()))
            .execute(pool)
            .await
            .expect("statement mutation probe");
    }
}

async fn no_writes(pool: &PgPool) {
    let called: bool = sqlx::query_scalar("SELECT is_called FROM inventory_write_probe")
        .fetch_one(pool)
        .await
        .expect("write probe receipt");
    assert!(
        !called,
        "inventory attempted a claim/sweep/repair/audit/write, even if rolled back or zero-row"
    );
}

fn interaction(options: Vec<CommandDataOption>) -> Interaction {
    let mut interaction = from_member(
        slash(COMMAND, Some(CHANNEL), options),
        ACTOR,
        Permissions::BAN_MEMBERS,
    );
    interaction.member.as_mut().expect("actor").roles = vec![Id::new(101)];
    interaction
}

async fn invoke(
    pool: PgPool,
    interaction: Interaction,
    authority: Authority,
    enabled: bool,
) -> (String, Vec<RestRequest>, Vec<Query>) {
    let rest = Rest::start(authority).await;
    let runtime = runtime_with_member(
        RouterGates {
            moderation: enabled,
            ..member_gates()
        },
        rest.origin.clone(),
        pool,
    );
    let (_, queries) = Queries::capture(runtime.on_interaction(&interaction)).await;
    let requests = rest.requests();
    let text = answer(&requests);
    (text, requests, queries)
}

fn answer(requests: &[RestRequest]) -> String {
    for request in requests {
        assert!(
            request.method == "GET"
                || (request.method == "POST" && request.path.ends_with("/callback"))
                || (request.method == "PATCH" && request.path.ends_with("/messages/@original")),
            "only authority GETs and the private interaction answer allowed: {} {}",
            request.method,
            request.path
        );
        if request.method == "GET" {
            assert!(
                !request.path.contains("/bans"),
                "inventory is saved state, not a Discord ban lookup"
            );
        }
    }
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|r| r.path.ends_with("/callback"))
        .collect();
    assert_eq!(callbacks.len(), 1, "exactly one private acknowledgement");
    let initial: Value = serde_json::from_slice(&callbacks[0].body).expect("callback JSON");
    assert_eq!(
        initial["data"]["flags"], 64,
        "ephemeral inventory and refusals"
    );
    let edits: Vec<_> = requests
        .iter()
        .filter(|r| r.path.ends_with("/messages/@original"))
        .collect();
    let body = if initial["type"] == 5 {
        assert_eq!(edits.len(), 1, "defer has one finite completion");
        serde_json::from_slice::<Value>(&edits[0].body).expect("completion JSON")
    } else {
        assert_eq!(initial["type"], 4);
        assert!(edits.is_empty());
        initial["data"].clone()
    };
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    let text = body["content"].as_str().expect("finite text answer");
    assert!(
        text.encode_utf16().count() <= 2000,
        "whole content, including framing, in UTF-16 units"
    );
    assert!(!two_bot_core::message_safety::contains_mass_mention(text));
    assert!(!text.contains("<@") && !text.contains("<#"));
    for secret in [
        PRIVATE,
        TOKEN,
        "foreign-expiry",
        "postgres://",
        "SQLSTATE",
        "private-actor-attribution",
    ] {
        assert!(
            !text.contains(secret),
            "private field/error/cross-guild leak"
        );
    }
    text.to_owned()
}

fn expiry_reads(queries: &[Query]) -> Vec<&Query> {
    queries
        .iter()
        .filter(|q| {
            let sql = q.normalized();
            sql.contains("moderation_scheduled_unbans") || sql.contains("moderation_member_bans")
        })
        .collect()
}

fn bounded_reads(queries: &[Query]) {
    let reads = expiry_reads(queries);
    assert!(
        !reads.is_empty(),
        "actual inventory query required, not a placeholder"
    );
    for read in reads {
        let sql = read.normalized();
        assert!(sql.contains("guild_id"), "guild fence in executed query");
        assert!(
            read.rows <= 11,
            "page + one lookahead maximum; never materialize all rows"
        );
        if !sql.contains("count(") {
            assert!(sql.contains("limit"), "LIMIT-bounded executed query");
            assert!(
                sql.contains("order by"),
                "store-owned deterministic ordering"
            );
        }
    }
}

fn saved_not_completed(text: &str) {
    let lower = text.to_lowercase();
    assert!(lower.contains("saved"), "identify saved state");
    assert!(
        lower.contains("unban") && (lower.contains("not") || lower.contains("doesn't")),
        "saved/due is not successful Discord unban: {text}"
    );
    assert!(!lower.contains("successfully unbanned"));
}

fn row<'a>(text: &'a str, request: &str, member: &str) -> &'a str {
    let line = text
        .lines()
        .find(|line| line.contains(request))
        .expect("complete stable work id on a row");
    assert!(
        line.contains(member),
        "complete member id bound to work identity: {line}"
    );
    line
}

#[tokio::test]
async fn every_saved_expiry_state_is_precise_private_and_mutation_free() {
    let db = database().await;
    for (request, member, state, uncertain) in [
        ("due-pending-work", "777700000000000001", "pending", false),
        (
            "future-pending-work",
            "777700000000000002",
            "pending",
            false,
        ),
        (
            "prepared-expiry-work",
            "777700000000000003",
            "staged",
            false,
        ),
        ("running-expiry-work", "777700000000000004", "running", true),
        (
            "quarantined-expiry-work",
            "777700000000000005",
            "quarantined",
            true,
        ),
        (
            "uncertain-pending-work",
            "777700000000000006",
            "pending",
            true,
        ),
    ] {
        seed(
            db.pool(),
            GUILD_S,
            request,
            member,
            state,
            if request == "future-pending-work" {
                "2099-01-01T00:00:00+00:00"
            } else {
                "2026-10-01T00:00:00+00:00"
            },
            uncertain,
        )
        .await;
    }
    seed(
        db.pool(),
        FOREIGN,
        "foreign-expiry",
        "999900000000000001",
        "pending",
        "2026-01-01T00:00:00+00:00",
        false,
    )
    .await;
    for state in ["done", "cancelled", "superseded"] {
        seed(
            db.pool(),
            GUILD_S,
            &format!("terminal-{state}"),
            &format!("member-{state}"),
            state,
            "2026-01-01T00:00:00+00:00",
            false,
        )
        .await;
    }
    let before = snapshot(db.pool()).await;
    install_write_probe(db.pool()).await;
    let (text, requests, queries) = invoke(
        db.pool().clone(),
        interaction(vec![]),
        Authority::Staff,
        true,
    )
    .await;
    assert!(row(&text, "due-pending-work", "777700000000000001")
        .to_lowercase()
        .contains("pending"));
    assert!(
        row(&text, "future-pending-work", "777700000000000002").contains("<t:4070908800:"),
        "localized saved future execute_at"
    );
    let prepared = row(&text, "prepared-expiry-work", "777700000000000003").to_lowercase();
    assert!(prepared.contains("staged") || prepared.contains("prepared"));
    assert!(row(&text, "running-expiry-work", "777700000000000004")
        .to_lowercase()
        .contains("running"));
    assert!(row(&text, "quarantined-expiry-work", "777700000000000005")
        .to_lowercase()
        .contains("quarantin"));
    assert!(
        row(&text, "uncertain-pending-work", "777700000000000006")
            .to_lowercase()
            .contains("uncertain"),
        "dispatch uncertainty cannot look like ordinary pending"
    );
    for state in ["done", "cancelled", "superseded"] {
        assert!(
            !text.contains(&format!("terminal-{state}")),
            "active expiry inventory excludes terminal history"
        );
    }
    saved_not_completed(&text);
    assert!(
        text.to_lowercase().contains("running") && text.to_lowercase().contains("complet"),
        "running explicitly is not completion"
    );
    assert!(
        requests
            .iter()
            .any(|r| r.method == "GET" && r.path.ends_with("/members/88")),
        "server-side actor recheck"
    );
    bounded_reads(&queries);
    assert_eq!(snapshot(db.pool()).await, before);
    no_writes(db.pool()).await;
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn multiple_pages_have_due_then_stable_identity_order_without_drops_or_writes() {
    let db = database().await;
    // Permuted insertion order, tied times, and opposing member/work ids expose unstable ties.
    for ordinal in 0..23 {
        let i = (ordinal * 7) % 23;
        seed(
            db.pool(),
            GUILD_S,
            &format!("page-work-{i:02}"),
            &format!("777700000000{:06}", 100 - i),
            "pending",
            "2026-10-01T00:00:00+00:00",
            false,
        )
        .await;
    }
    seed(
        db.pool(),
        GUILD_S,
        "earlier-work",
        "777700000000999999",
        "pending",
        "2026-09-30T00:00:00+00:00",
        false,
    )
    .await;
    let before = snapshot(db.pool()).await;
    install_write_probe(db.pool()).await;
    let mut seen = Vec::new();
    for page in 1..=3 {
        let (text, _, queries) = invoke(
            db.pool().clone(),
            interaction(vec![option("page", CommandOptionValue::Integer(page))]),
            Authority::Staff,
            true,
        )
        .await;
        saved_not_completed(&text);
        assert!(
            text.to_lowercase().contains("page"),
            "selected page discoverable"
        );
        let identities: Vec<_> = text
            .lines()
            .filter_map(|line| {
                if line.contains("earlier-work") {
                    Some("earlier-work".to_owned())
                } else {
                    (0..23)
                        .map(|i| format!("page-work-{i:02}"))
                        .find(|id| line.contains(id))
                }
            })
            .collect();
        assert_eq!(
            identities.len(),
            if page < 3 { 10 } else { 4 },
            "ten complete rows except final page"
        );
        for identity in &identities {
            if identity == "earlier-work" {
                row(&text, identity, "777700000000999999");
            } else {
                let i: u64 = identity
                    .strip_prefix("page-work-")
                    .expect("fixture id")
                    .parse()
                    .expect("fixture index");
                row(&text, identity, &format!("777700000000{:06}", 100 - i));
            }
        }
        bounded_reads(&queries);
        seen.extend(identities);
    }
    assert_eq!(seen[0], "earlier-work", "due time outranks stable identity");
    let forward: Vec<_> = (0..23).map(|i| format!("page-work-{i:02}")).collect();
    let reverse: Vec<_> = forward.iter().rev().cloned().collect();
    assert!(
        seen[1..] == forward || seen[1..] == reverse,
        "ties use one documented stable member/work order, not insertion/clock order"
    );
    let (repeat, _, _) = invoke(
        db.pool().clone(),
        interaction(vec![option("page", CommandOptionValue::Integer(1))]),
        Authority::Staff,
        true,
    )
    .await;
    assert!(repeat.contains("earlier-work"));
    for identity in &seen[..10] {
        assert!(repeat.contains(identity), "repeat page preserves ties");
    }
    let (default_page, _, _) = invoke(
        db.pool().clone(),
        interaction(vec![]),
        Authority::Staff,
        true,
    )
    .await;
    for identity in &seen {
        assert_eq!(
            default_page.contains(identity),
            repeat.contains(identity),
            "omitted page selects the same complete identities as page one"
        );
    }
    assert_eq!(snapshot(db.pool()).await, before);
    no_writes(db.pool()).await;
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn malformed_selectors_are_corrected_before_any_expiry_read() {
    let db = database().await;
    seed(
        db.pool(),
        GUILD_S,
        "never-read-work",
        "777700000000000001",
        "pending",
        "2026-10-01T00:00:00+00:00",
        false,
    )
    .await;
    let before = snapshot(db.pool()).await;
    install_write_probe(db.pool()).await;
    for options in [
        vec![option("page", CommandOptionValue::Integer(0))],
        vec![option("page", CommandOptionValue::Integer(-1))],
        vec![option("page", CommandOptionValue::Integer(10001))],
        vec![option("page", CommandOptionValue::Integer(i64::MAX))],
        vec![option("page", CommandOptionValue::String("1".into()))],
        vec![option("page", CommandOptionValue::Boolean(true))],
        vec![option("page", CommandOptionValue::SubCommand(vec![]))],
        vec![
            option("page", CommandOptionValue::Integer(1)),
            option("page", CommandOptionValue::Integer(2)),
        ],
        vec![option("target", CommandOptionValue::User(Id::new(ACTOR)))],
        vec![option("retry", CommandOptionValue::Boolean(true))],
    ] {
        let (text, _, queries) = invoke(
            db.pool().clone(),
            interaction(options),
            Authority::Staff,
            true,
        )
        .await;
        assert!(
            expiry_reads(&queries).is_empty(),
            "reject malformed shape before reading"
        );
        assert!(!text.contains("never-read-work"));
        let lower = text.to_lowercase();
        assert!(
            lower.contains(COMMAND) && lower.contains("page"),
            "finite corrective invocation, not generic unavailable: {text}"
        );
        assert!(text.encode_utf16().count() <= 500);
    }
    let (ceiling, _, queries) = invoke(
        db.pool().clone(),
        interaction(vec![option("page", CommandOptionValue::Integer(10_000))]),
        Authority::Staff,
        true,
    )
    .await;
    assert!(
        !expiry_reads(&queries).is_empty(),
        "published inclusive page ceiling is valid; an empty high page still checks storage"
    );
    assert!(ceiling.contains(COMMAND) && ceiling.to_lowercase().contains("page"));
    assert_eq!(snapshot(db.pool()).await, before);
    no_writes(db.pool()).await;
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn stale_nonstaff_missing_and_cold_actor_never_reads_or_mutates_inventory() {
    let db = database().await;
    seed(
        db.pool(),
        GUILD_S,
        "staff-only-expiry",
        "777700000000000001",
        "pending",
        "2026-10-01T00:00:00+00:00",
        false,
    )
    .await;
    let before = snapshot(db.pool()).await;
    install_write_probe(db.pool()).await;
    for authority in [Authority::Ordinary, Authority::Missing, Authority::Cold] {
        let (text, requests, queries) =
            invoke(db.pool().clone(), interaction(vec![]), authority, true).await;
        assert!(
            requests.iter().any(|r| r.method == "GET"),
            "stale interaction bit cannot replace current authority"
        );
        assert!(
            expiry_reads(&queries).is_empty(),
            "no inventory read without complete current staff authority"
        );
        assert!(!text.contains("staff-only-expiry"));
        let lower = text.to_lowercase();
        assert!(
            lower.contains("permission")
                || lower.contains("staff")
                || lower.contains("try")
                || lower.contains("operator"),
            "finite authority correction"
        );
    }
    assert_eq!(snapshot(db.pool()).await, before);
    no_writes(db.pool()).await;
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn guild_moderation_and_member_self_boundaries_refuse_before_reads() {
    for case in ["disabled", "foreign", "dm", "nonstaff", "missing-actor"] {
        let mut request = interaction(vec![]);
        match case {
            "foreign" => request.guild_id = Some(Id::new(9999)),
            "dm" => request.guild_id = None,
            "nonstaff" => {
                request.member.as_mut().expect("actor").permissions = Some(Permissions::empty())
            }
            "missing-actor" => {
                request.member = None;
                request.user = None;
            }
            _ => {}
        }
        let (_, requests, queries) =
            invoke(lazy_pool(), request, Authority::Staff, case != "disabled").await;
        assert!(expiry_reads(&queries).is_empty());
        assert!(
            requests
                .iter()
                .all(|r| r.path.ends_with("/callback") || r.path.ends_with("/messages/@original")),
            "routing refusal never fetches authority or Discord state"
        );
    }
}

#[tokio::test]
async fn empty_out_of_range_and_query_failure_are_distinct_with_finite_next_actions() {
    let db = database().await;
    seed(
        db.pool(),
        FOREIGN,
        "foreign-expiry",
        "999900000000000001",
        "pending",
        "2026-10-01T00:00:00+00:00",
        false,
    )
    .await;
    let (empty, _, _) = invoke(
        db.pool().clone(),
        interaction(vec![]),
        Authority::Staff,
        true,
    )
    .await;
    let lower = empty.to_lowercase();
    assert!(
        lower.contains("no saved")
            || lower.contains("no active")
            || lower.contains("no temporary")
            || lower.contains("empty"),
        "truthful empty saved set: {empty}"
    );
    assert!(
        lower.contains(COMMAND),
        "empty has finite refresh invocation"
    );
    seed(
        db.pool(),
        GUILD_S,
        "one-work",
        "777700000000000001",
        "pending",
        "2026-10-01T00:00:00+00:00",
        false,
    )
    .await;
    let (range, _, _) = invoke(
        db.pool().clone(),
        interaction(vec![option("page", CommandOptionValue::Integer(2))]),
        Authority::Staff,
        true,
    )
    .await;
    assert_ne!(range, empty, "out-of-range is not empty inventory");
    assert!(range.contains(COMMAND) && range.to_lowercase().contains("page"));
    let closed = db
        .independent_pool()
        .await
        .expect("independent guarded pool");
    closed.close().await;
    let (failure, _, _) = invoke(closed, interaction(vec![]), Authority::Staff, true).await;
    assert_ne!(failure, empty, "unknown is not empty");
    assert_ne!(failure, range);
    assert!(
        failure.to_lowercase().contains("unavailable") || failure.to_lowercase().contains("failed")
    );
    assert!(failure.to_lowercase().contains("try") || failure.to_lowercase().contains("operator"));
    db.close().await.expect("fixture teardown");
}

#[tokio::test]
async fn hostile_full_work_ids_are_safe_and_readonly_pool_still_returns_saved_rows() {
    let db = database().await;
    let work = "expiry-🦀-```-@everyone-<@123>-tail";
    seed(
        db.pool(),
        GUILD_S,
        work,
        "777700000000000001",
        "pending",
        "2026-10-01T00:00:00+00:00",
        false,
    )
    .await;
    let before = snapshot(db.pool()).await;
    let readonly = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET default_transaction_read_only = on")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(db.pool().connect_options().as_ref().clone())
        .await
        .expect("read-only fixture pool");
    let (text, _, queries) = invoke(
        readonly.clone(),
        interaction(vec![]),
        Authority::Staff,
        true,
    )
    .await;
    assert!(text.contains("777700000000000001"));
    assert!(
        text.contains("expiry-") && text.contains("tail"),
        "safe encoding keeps whole stable identity, never clipped"
    );
    saved_not_completed(&text);
    bounded_reads(&queries);
    assert_eq!(snapshot(db.pool()).await, before);
    readonly.close().await;
    db.close().await.expect("fixture teardown");
}

/// Protected publication contract: the inventory must be discoverable as a
/// private staff moderation command with bounded page selectors. Lives in the
/// unit-test module (not `crates/core/tests/`) so hosted CI observes it in the
/// same lib/bins step as the runtime tests; the workspace integration step is
/// skipped while any unit test is RED by design.
#[test]
fn tempban_inventory_is_a_private_staff_moderation_command_with_bounded_pages() {
    let definitions = two_bot_core::moderation::moderation_commands();
    let definition = definitions
        .iter()
        .find(|definition| definition.name == "tempban-list")
        .expect("saved temporary-ban expiry inventory must be discoverable as /tempban-list");
    assert!(!definition.dm_permission, "guild-only inventory");
    assert_eq!(
        definition.default_member_permissions,
        Some(two_bot_core::commands::PERM_BAN_MEMBERS.to_string()),
        "Ban Members, not self/member or generic server-management authority"
    );
    assert_eq!(definition.options.len(), 1, "page only; no action selector");
    let page = &definition.options[0];
    assert_eq!(page.name, "page");
    assert_eq!(
        page.kind,
        two_bot_core::commands::CommandOptionType::Integer.as_u8()
    );
    assert_ne!(page.required, Some(true), "omitted page selects page one");
    assert_eq!(page.min_value, Some(1));
    assert_eq!(page.max_value, Some(10_000));
    assert!(page.options.is_empty(), "no nested mutation surface");
    let help = definition.help_detail_text().to_lowercase();
    assert!(help.contains("page"), "help explains published paging");
    assert!(
        help.contains("saved") && (help.contains("expiry") || help.contains("expir")),
        "discovery describes saved expiry work, not observed unbans"
    );
}

#[test]
fn existing_tempban_mutation_definition_remains_a_positive_control() {
    let definitions = two_bot_core::moderation::moderation_commands();
    let definition = definitions
        .iter()
        .find(|definition| definition.name == "tempban")
        .expect("existing tempban mutation is retained");
    assert_eq!(
        definition.default_member_permissions,
        Some(two_bot_core::commands::PERM_BAN_MEMBERS.to_string())
    );
    assert!(!definition.dm_permission);
    for name in ["target", "duration_seconds", "reason"] {
        assert!(
            definition
                .options
                .iter()
                .any(|option| option.name == name && option.required == Some(true)),
            "existing required input {name} remains"
        );
    }
}
