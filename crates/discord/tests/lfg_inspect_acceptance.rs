//! Protected failing-first contract for private, current-channel LFG inspection.
//! Wire contract: /lfg-inspect id:<saved ID> [role:<1..20>] [page:<1..100>].
//! Roles use position/key order; members use joined_at/user_id order. Page sizes
//! are implementation-owned: these tests follow whole rows until an honest end.
#![cfg(feature = "db")]
#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use futures_util::{future::BoxFuture, FutureExt};
use serde_json::{json, Value};
use std::panic::AssertUnwindSafe;
use twilight_model::application::interaction::Interaction;
use two_bot_core::{commands::PERM_MANAGE_EVENTS, lfg, lfg_store, RouterGates};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "2222";
const CHANNEL: &str = "4444";
const POST: &str = "inspect-fixture";
const START: &str = "2099-01-02T03:04:05Z";
const START_EPOCH: &str = "4071006245";
const TOKEN: &str = "inspection-fixture-token";

fn gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(2222),
        announcements: true,
        scorecard: false,
        automations: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn invocation(id: u64, command: &str, options: Vec<Value>) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
        "member": {"user": {"id": "3333", "username": "host", "discriminator": "0", "avatar": null},
            "roles": [], "joined_at": null, "deaf": false, "mute": false,
            "flags": 0, "permissions": PERM_MANAGE_EVENTS.to_string()},
        "token": TOKEN, "version": 1, "entitlements": [],
        "authorizing_integration_owners": {},
        "data": {"id": "1", "name": command, "type": 1, "options": options}
    }))
    .expect("synthetic interaction")
}

fn inspect(id: u64, post: &str, role: Option<i64>, page: Option<i64>) -> Interaction {
    let mut options = vec![json!({"type": 3, "name": "id", "value": post})];
    if let Some(role) = role {
        options.push(json!({"type": 4, "name": "role", "value": role}));
    }
    if let Some(page) = page {
        options.push(json!({"type": 4, "name": "page", "value": page}));
    }
    invocation(id, "lfg-inspect", options)
}

fn alter(event: Interaction, change: impl FnOnce(&mut Value)) -> Interaction {
    let mut value = serde_json::to_value(event).expect("wire interaction");
    change(&mut value);
    serde_json::from_value(value).expect("altered interaction")
}

fn runtime(pool: sqlx::PgPool, mock: &MockRest, active: RouterGates) -> InteractionRuntime {
    InteractionRuntime::new(
        active,
        pool,
        ActionExecutor::with_proxy(TOKEN.into(), Some(mock.origin())).expect("loopback executor"),
        1111,
        two_bot_core::ClassifierConfig::default(),
    )
}

async fn with_database<F>(case: F)
where
    F: for<'a> FnOnce(&'a sqlx::PgPool) -> BoxFuture<'a, ()>,
{
    // Full migrations are large; keep one owned fixture alive even when CI
    // runs this integration binary with its normal parallel test threads.
    static FIXTURES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let _permit = FIXTURES.acquire().await.expect("fixture semaphore open");
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable fixture");
    let result = AssertUnwindSafe(case(db.pool())).catch_unwind().await;
    db.close().await.expect("explicit fixture teardown");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn seed_post(pool: &sqlx::PgPool, id: &str, guild: &str, channel: &str) {
    sqlx::query("INSERT INTO lfg_posts
        (id,guild_id,channel_id,message_id,title,starts_at,status,created_by,created_at)
        VALUES ($1,$2,$3,'8000','Saved raid title',$4::timestamptz,'open','987654321','2020-01-01T00:00:00Z')")
        .bind(id).bind(guild).bind(channel).bind(START)
        .execute(pool).await.expect("seed saved post");
}

fn filled(role: usize) -> usize {
    if role == 0 || role == 19 {
        99
    } else {
        role
    }
}

fn member(role: usize, ordinal: usize) -> String {
    (70_000_000_000_000_000_u64 + role as u64 * 1000 + ordinal as u64).to_string()
}

async fn seed_roster(pool: &sqlx::PgPool) {
    seed_post(pool, POST, GUILD, CHANNEL).await;
    // Reverse insertion exposes ordering; saved positions are unique by schema.
    sqlx::query(
        "INSERT INTO lfg_roles (lfg_id,role_key,label,slots,position)
        SELECT $1,'r'||lpad(n::text,2,'0'),'RosterRole'||lpad(n::text,2,'0'),99,n
        FROM generate_series(0,19) AS n ORDER BY n DESC",
    )
    .bind(POST)
    .execute(pool)
    .await
    .expect("seed twenty configured roles");
    // Equal join times expose the user-ID tie-break independently of insertion order.
    sqlx::query(
        "INSERT INTO lfg_signups (lfg_id,user_id,role_key,joined_at)
        SELECT $1,(70000000000000000::bigint + r*1000 + u)::text,
            'r'||lpad(r::text,2,'0'),'2020-01-02T03:04:05Z'::timestamptz
        FROM generate_series(0,19) AS r CROSS JOIN generate_series(1,99) AS u
        WHERE u <= CASE WHEN r IN (0,19) THEN 99 ELSE r END
        ORDER BY r DESC,u DESC",
    )
    .bind(POST)
    .execute(pool)
    .await
    .expect("seed complete saved roster");
}

async fn snapshot(pool: &sqlx::PgPool) -> Value {
    // xmin detects even UPDATE-to-the-same-value, including delivery/claim fields.
    sqlx::query_scalar("SELECT jsonb_build_object(
        'posts',(SELECT jsonb_agg(to_jsonb(p)||jsonb_build_object('_xmin',p.xmin::text) ORDER BY p.id) FROM lfg_posts p),
        'roles',(SELECT jsonb_agg(to_jsonb(r)||jsonb_build_object('_xmin',r.xmin::text) ORDER BY r.lfg_id,r.role_key) FROM lfg_roles r),
        'signups',(SELECT jsonb_agg(to_jsonb(s)||jsonb_build_object('_xmin',s.xmin::text) ORDER BY s.lfg_id,s.user_id) FROM lfg_signups s),
        'audit',(SELECT jsonb_agg(to_jsonb(a)||jsonb_build_object('_xmin',a.xmin::text) ORDER BY a.id) FROM announcements_audit_log a))")
        .fetch_one(pool).await.expect("snapshot durable state")
}

fn body(request: &RestRequest) -> Value {
    serde_json::from_slice(&request.body).expect("Discord JSON")
}

struct Reply {
    handled: bool,
    requests: Vec<RestRequest>,
    content: String,
}

async fn handle(rt: &InteractionRuntime, mock: &MockRest, event: &Interaction) -> Reply {
    let start = mock.requests().len();
    let handled = rt.handle(event).await.expect("bounded interaction result");
    let requests = mock.requests()[start..].to_vec();
    let content = requests
        .last()
        .map(body)
        .and_then(|v| {
            v.get("content")
                .or_else(|| v.get("data").and_then(|d| d.get("content")))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    Reply {
        handled,
        requests,
        content,
    }
}

fn assert_private(reply: &Reply) {
    assert!(
        reply.handled,
        "/lfg-inspect must be registered and dispatched"
    );
    assert!(
        !reply.requests.is_empty(),
        "inspection needs an explicit private result"
    );
    assert!(
        reply.requests.len() <= 2,
        "one callback and at most one edit, not a second acknowledgement"
    );
    for (n, request) in reply.requests.iter().enumerate() {
        let payload = body(request);
        let data = if n == 0 {
            assert_eq!(request.method, "POST");
            assert!(
                request.path.starts_with("/api/v10/interactions/")
                    && request.path.ends_with("/callback")
            );
            assert!([4, 5].contains(&payload["type"].as_u64().expect("callback type")));
            assert_eq!(
                payload["data"]["flags"], 64,
                "inspection is always ephemeral"
            );
            &payload["data"]
        } else {
            assert_eq!(request.method, "PATCH");
            assert_eq!(
                request.path,
                format!("/api/v10/webhooks/1111/{TOKEN}/messages/@original")
            );
            &payload
        };
        if data.get("content").is_some() {
            assert_eq!(data["allowed_mentions"]["parse"], json!([]));
            for key in ["users", "roles"] {
                assert!(
                    data["allowed_mentions"]
                        .get(key)
                        .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)),
                    "no explicit ping allowlist"
                );
            }
        }
        for key in ["embeds", "components", "attachments"] {
            assert!(
                data.get(key)
                    .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)),
                "inspection is read-only text, not an actionable public control: {key}"
            );
        }
    }
    assert!(!reply.content.is_empty());
    assert!(
        reply.content.encode_utf16().count() <= 2000,
        "UTF-16 ceiling"
    );
}

async fn read(
    rt: &InteractionRuntime,
    mock: &MockRest,
    pool: &sqlx::PgPool,
    event: Interaction,
) -> Reply {
    let before = snapshot(pool).await;
    let reply = handle(rt, mock, &event).await;
    assert_eq!(
        before,
        snapshot(pool).await,
        "inspection cannot write signup, cooldown receipt, claim, post or audit state"
    );
    assert_private(&reply);
    reply
}

async fn closed_pool() -> sqlx::PgPool {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_local")
        .expect("disposable lazy endpoint");
    pool.close().await;
    pool
}

fn assert_end(content: &str) {
    let lower = content.to_lowercase();
    assert!(
        lower.contains("page") && (lower.contains("out of range") || lower.contains("no more")),
        "honest out-of-range page with recovery: {content}"
    );
    assert!(
        content.contains("/lfg-inspect"),
        "give a usable next action"
    );
}

fn role_rows(content: &str) -> Vec<usize> {
    content
        .lines()
        .filter_map(|line| {
            let (_, tail) = line.split_once("RosterRole")?;
            Some(
                tail.get(..2)
                    .expect("whole role label")
                    .parse()
                    .expect("role index"),
            )
        })
        .collect()
}

fn member_rows(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            line.split(|c: char| !c.is_ascii_digit())
                .find(|part| part.len() == 17 && part.starts_with("700000000000"))
                .map(str::to_owned)
        })
        .collect()
}

#[tokio::test]
async fn summaries_reach_every_role_with_truthful_counts_without_public_truncation() {
    with_database(|pool| {
        Box::pin(async move {
            seed_roster(pool).await;
            let post = lfg_store::get_lfg(pool, GUILD, POST)
                .await
                .expect("saved post")
                .expect("post");
            let roles = lfg_store::list_lfg_roles(pool, POST).await.expect("roles");
            let signups = lfg_store::list_lfg_signups(pool, POST)
                .await
                .expect("signups");
            let public = lfg::lfg_content(&post, &roles, &signups);
            assert!(public.encode_utf16().count() <= 2000);
            assert!(
                !public.contains("RosterRole19"),
                "fixture must demonstrate public-post truncation"
            );
            let mock =
                MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"8000"}))).await;
            let rt = runtime(pool.clone(), &mock, gates());
            let mut seen = Vec::new();
            let mut ended = false;
            for page in 1..=21 {
                let reply = read(
                    &rt,
                    &mock,
                    pool,
                    inspect(10_000 + page as u64, POST, None, Some(page)),
                )
                .await;
                let rows = role_rows(&reply.content);
                if rows.is_empty() {
                    assert_end(&reply.content);
                    ended = true;
                    break;
                }
                assert!(reply.content.contains(POST) && reply.content.contains("Saved raid title"));
                assert!(reply.content.contains(&format!("<t:{START_EPOCH}:f>")));
                assert!(reply.content.to_lowercase().contains("open"));
                assert!(
                    reply.content.to_lowercase().contains("saved"),
                    "stored state, not live delivery proof"
                );
                assert!(
                    !reply.content.contains("https://discord.com/channels/"),
                    "a stored message ID is not a confirmed jump target"
                );
                for role in &rows {
                    let label = format!("RosterRole{role:02}");
                    let line = reply
                        .content
                        .lines()
                        .find(|line| line.contains(&label))
                        .expect("whole summary");
                    assert!(
                        line.contains(&format!("{}/99", filled(*role))),
                        "filled/total in {line}"
                    );
                    assert!(
                        line.contains(&format!("{} remaining", 99 - filled(*role))),
                        "remaining in {line}"
                    );
                }
                assert!(
                    member_rows(&reply.content).is_empty(),
                    "summaries do not disclose an unselected roster"
                );
                let repeat = read(
                    &rt,
                    &mock,
                    pool,
                    inspect(11_000 + page as u64, POST, None, Some(page)),
                )
                .await;
                assert_eq!(
                    reply.content, repeat.content,
                    "deterministic repeat of the same saved page"
                );
                seen.extend(rows);
            }
            assert!(ended, "bounded traversal must terminate");
            assert_eq!(
                seen,
                (0..20).collect::<Vec<_>>(),
                "all configured roles exactly once, position/key order"
            );
            drop(rt);
            mock.shutdown().await;
        })
    })
    .await;
}

#[tokio::test]
async fn every_selected_role_roster_is_reachable_once_in_join_time_and_id_order() {
    with_database(|pool| {
        Box::pin(async move {
            seed_roster(pool).await;
            let mock =
                MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"8000"}))).await;
            let rt = runtime(pool.clone(), &mock, gates());
            for role in 0..20 {
                let mut seen = Vec::new();
                let mut ended = false;
                for page in 1..=100 {
                    let event = inspect(
                        20_000 + role as u64 * 100 + page as u64,
                        POST,
                        Some(role as i64 + 1),
                        Some(page),
                    );
                    let reply = read(&rt, &mock, pool, event).await;
                    let rows = member_rows(&reply.content);
                    if rows.is_empty() {
                        assert_end(&reply.content);
                        ended = true;
                        break;
                    }
                    assert!(
                        reply.content.contains(&format!("RosterRole{role:02}")),
                        "selected role identified"
                    );
                    assert!(reply.content.contains(POST));
                    for row in &rows {
                        assert_eq!(
                            reply.content.matches(row).count(),
                            1,
                            "no duplicated member on a page"
                        );
                    }
                    let repeat = read(
                        &rt,
                        &mock,
                        pool,
                        inspect(
                            30_000 + role as u64 * 100 + page as u64,
                            POST,
                            Some(role as i64 + 1),
                            Some(page),
                        ),
                    )
                    .await;
                    assert_eq!(reply.content, repeat.content);
                    seen.extend(rows);
                }
                assert!(ended, "bounded roster traversal");
                assert_eq!(
                    seen,
                    (1..=filled(role))
                        .map(|n| member(role, n))
                        .collect::<Vec<_>>(),
                    "role {}: every saved signup exactly once, no foreign-role rows",
                    role + 1
                );
            }
            drop(rt);
            mock.shutdown().await;
        })
    })
    .await;
}

#[tokio::test]
async fn closed_empty_and_never_confirmed_posts_remain_honest_saved_inspections() {
    with_database(|pool| Box::pin(async move {
        seed_post(pool,POST,GUILD,CHANNEL).await;
        sqlx::query("INSERT INTO lfg_roles (lfg_id,role_key,label,slots,position) VALUES ($1,'empty','Empty role',3,0)")
            .bind(POST).execute(pool).await.expect("empty role fixture");
        sqlx::query("UPDATE lfg_posts SET status='closed',closed_at=now(),message_id=NULL WHERE id=$1")
            .bind(POST).execute(pool).await.expect("closed unconfirmed fixture");
        let mock = MockRest::start(vec![],ScriptedResponse::json(200,json!({"id":"8000"}))).await;
        let rt = runtime(pool.clone(),&mock,gates());
        let summary = read(&rt,&mock,pool,inspect(40_001,POST,None,None)).await;
        for part in [POST,"Saved raid title","Empty role","0/3","3 remaining","closed"] { assert!(summary.content.contains(part),"missing {part}: {}",summary.content); }
        assert!(summary.content.to_lowercase().contains("saved"));
        assert!(!summary.content.contains("https://discord.com/channels/"));
        let roster = read(&rt,&mock,pool,inspect(40_002,POST,Some(1),None)).await;
        assert!(roster.content.to_lowercase().contains("no saved signups"),"empty is not missing or an outage: {}",roster.content);
        assert!(member_rows(&roster.content).is_empty());
        let later = read(&rt,&mock,pool,inspect(40_003,POST,Some(1),Some(2))).await;
        assert_end(&later.content);
        let maximum_page = read(&rt,&mock,pool,inspect(40_004,POST,None,Some(100))).await;
        assert_end(&maximum_page.content);
        let absent_role = read(&rt,&mock,pool,inspect(40_005,POST,Some(2),None)).await;
        let lower = absent_role.content.to_lowercase();
        assert!(lower.contains("role") && (lower.contains("out of range") || lower.contains("not found") || lower.contains("invalid")), "configured-role bounds, not an empty saved roster: {}", absent_role.content);
        assert!(!lower.contains("no saved signups"));
        drop(rt);
        mock.shutdown().await;
    })).await;
}

#[tokio::test]
async fn hostile_labels_and_maximum_unicode_rows_are_inert_and_not_cut_in_half() {
    with_database(|pool| {
        Box::pin(async move {
            seed_roster(pool).await;
            let unicode = "🦀".repeat(35);
            for role in 0..20 {
                sqlx::query("UPDATE lfg_roles SET label=$1 WHERE lfg_id=$2 AND role_key=$3")
                    .bind(format!("RosterRole{role:02}{unicode}"))
                    .bind(POST)
                    .bind(format!("r{role:02}"))
                    .execute(pool)
                    .await
                    .expect("maximum Unicode label");
            }
            let hostile =
                "@everyone <@123> <@&456> **bait** `code` [link](https://evil.invalid)\n# title";
            sqlx::query(
                "UPDATE lfg_posts SET title=$1,message_id='8000/../../@everyone' WHERE id=$2",
            )
            .bind(hostile)
            .bind(POST)
            .execute(pool)
            .await
            .expect("hostile saved title and message");
            let mock =
                MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"8000"}))).await;
            let rt = runtime(pool.clone(), &mock, gates());
            let mut seen = Vec::new();
            for page in 1..=21 {
                let reply = read(
                    &rt,
                    &mock,
                    pool,
                    inspect(41_000 + page as u64, POST, None, Some(page)),
                )
                .await;
                let rows = role_rows(&reply.content);
                if rows.is_empty() {
                    assert_end(&reply.content);
                    break;
                }
                assert_eq!(
                    reply.content.matches('🦀').count(),
                    35 * rows.len(),
                    "never truncate or duplicate a role label"
                );
                for raw in [
                    "@everyone",
                    "<@123>",
                    "<@&456>",
                    "**bait**",
                    "`code`",
                    "[link](https://evil.invalid)",
                    "\n# title",
                ] {
                    assert!(
                        !reply.content.contains(raw),
                        "hostile saved text remained active: {raw}"
                    );
                }
                assert!(
                    reply.content.contains("bait")
                        && reply.content.contains("code")
                        && reply.content.contains("title")
                );
                seen.extend(rows);
            }
            assert_eq!(seen, (0..20).collect::<Vec<_>>());
            drop(rt);
            mock.shutdown().await;
        })
    })
    .await;
}

#[tokio::test]
async fn exact_utf16_id_boundaries_and_default_page_are_accepted_without_shortening_ids() {
    with_database(|pool| Box::pin(async move {
        let mock = MockRest::start(vec![],ScriptedResponse::json(200,json!({"id":"8000"}))).await;
        let rt = runtime(pool.clone(),&mock,gates());
        for (n,id) in ["a".repeat(128),"🦀".repeat(64)].iter().enumerate() {
            seed_post(pool,id,GUILD,CHANNEL).await;
            sqlx::query("INSERT INTO lfg_roles (lfg_id,role_key,label,slots,position) VALUES ($1,'tank','Boundary role',2,0)")
                .bind(id).execute(pool).await.expect("boundary role");
            let implicit = read(&rt,&mock,pool,inspect(41_100+n as u64,id,None,None)).await;
            let explicit = read(&rt,&mock,pool,inspect(41_110+n as u64,id,None,Some(1))).await;
            assert_eq!(implicit.content,explicit.content,"default page is first page");
            assert!(implicit.content.contains(id.as_str()),"full management identity survives output escaping");
            assert!(implicit.content.contains("Boundary role") && implicit.content.contains("0/2") && implicit.content.contains("2 remaining"));
        }
        drop(rt); mock.shutdown().await;
    })).await;
}

#[tokio::test]
async fn malformed_duplicate_and_unbounded_options_refuse_before_storage() {
    let pool = closed_pool().await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"8000"}))).await;
    let rt = runtime(pool, &mock, gates());
    let valid_id = json!({"type":3,"name":"id","value":POST});
    let mut invalid = vec![
        vec![],
        vec![json!({"type":4,"name":"id","value":1})],
        vec![valid_id.clone(), valid_id.clone()],
    ];
    for id in [
        "".to_owned(),
        " ".to_owned(),
        "x".repeat(129),
        "🦀".repeat(65),
        format!("{}@everyone", "x".repeat(129)),
    ] {
        invalid.push(vec![json!({"type":3,"name":"id","value":id})]);
    }
    for (name, max) in [("role", 20), ("page", 100)] {
        for value in [0, -1, max + 1, i64::MAX] {
            invalid.push(vec![
                valid_id.clone(),
                json!({"type":4,"name":name,"value":value}),
            ]);
        }
        for value in [json!("1"), json!("@everyone"), json!(true)] {
            invalid.push(vec![
                valid_id.clone(),
                json!({"type":if value.is_boolean(){5}else{3},"name":name,"value":value}),
            ]);
        }
        invalid.push(vec![
            valid_id.clone(),
            json!({"type":10,"name":name,"value":1.5}),
        ]);
        invalid.push(vec![
            valid_id.clone(),
            json!({"type":4,"name":name,"value":1}),
            json!({"type":4,"name":name,"value":1}),
        ]);
    }
    invalid.push(vec![
        valid_id,
        json!({"type":3,"name":"channel","value":"5555"}),
    ]);
    for (n, options) in invalid.into_iter().enumerate() {
        let reply = handle(
            &rt,
            &mock,
            &invocation(42_000 + n as u64, "lfg-inspect", options),
        )
        .await;
        assert_private(&reply);
        let lower = reply.content.to_lowercase();
        assert!(
            lower.contains("invalid") || lower.contains("must be"),
            "invalid input reached the failed store: {}",
            reply.content
        );
        assert!(!lower.contains("unable") && !lower.contains("unavailable"));
        assert!(!reply.content.contains("@everyone"));
    }
    drop(rt);
    mock.shutdown().await;
}

#[tokio::test]
async fn authority_feature_and_guild_fences_precede_a_failed_store() {
    let pool = closed_pool().await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"8000"}))).await;
    let rt = runtime(pool.clone(), &mock, gates());
    for id in [43_001, 43_002] {
        let reply = handle(
            &rt,
            &mock,
            &alter(inspect(id, POST, None, None), |v| {
                v["member"]["permissions"] = json!("0")
            }),
        )
        .await;
        assert_private(&reply);
        assert_eq!(
            reply.content,
            two_bot_core::MANAGE_EVENTS_REQUIRED,
            "revalidate permission on every invocation"
        );
    }
    let mut disabled = gates();
    disabled.announcements = false;
    let off = runtime(pool, &mock, disabled);
    let reply = handle(&off, &mock, &inspect(43_003, POST, None, None)).await;
    assert_private(&reply);
    assert_eq!(reply.content, two_bot_core::ANNOUNCEMENTS_DISABLED_REPLY);
    let foreign = handle(
        &rt,
        &mock,
        &alter(inspect(43_004, POST, None, None), |v| {
            v["guild_id"] = json!("6666")
        }),
    )
    .await;
    assert!(
        foreign.requests.is_empty(),
        "foreign configured guild ignored before I/O"
    );
    let dm = alter(inspect(43_005, POST, None, None), |v| {
        v["user"] = v["member"]["user"].clone();
        v.as_object_mut().expect("object").remove("guild_id");
        v.as_object_mut().expect("object").remove("member");
    });
    assert!(handle(&rt, &mock, &dm).await.requests.is_empty());
    drop(off);
    drop(rt);
    mock.shutdown().await;
}

#[tokio::test]
async fn store_outage_is_not_empty_missing_or_a_discord_history_fallback() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"8000"}))).await;
    let rt = runtime(closed_pool().await, &mock, gates());
    let reply = handle(&rt, &mock, &inspect(44_001, POST, None, None)).await;
    assert_private(&reply);
    let lower = reply.content.to_lowercase();
    assert!(
        lower.contains("unable") || lower.contains("unavailable"),
        "truthful bounded outage: {}",
        reply.content
    );
    assert!(lower.contains("try") && reply.content.contains("/lfg-inspect"));
    for raw in ["postgres", "sqlx", "agent_test", "No saved signups"] {
        assert!(!reply.content.contains(raw));
    }
    drop(rt);
    mock.shutdown().await;
}

async fn install_read_spy(pool: &sqlx::PgPool) {
    // This sequence is disposable test instrumentation, not an application write.
    // Projecting the WHERE key through a VOLATILE function observes even a read
    // for a missing ID, without relying on the planner's outer LIMIT behavior.
    for statement in [
        "CREATE SEQUENCE inspection_roster_reads",
        "CREATE FUNCTION inspection_read_probe(value text) RETURNS text LANGUAGE plpgsql VOLATILE AS $$ BEGIN PERFORM nextval('inspection_roster_reads'); RETURN value; END $$",
        "ALTER TABLE lfg_roles RENAME TO inspection_roles_fixture",
        "ALTER TABLE lfg_signups RENAME TO inspection_signups_fixture",
        "CREATE VIEW lfg_roles WITH (security_barrier=true) AS SELECT inspection_read_probe(lfg_id) AS lfg_id,role_key,label,slots,position FROM inspection_roles_fixture",
        "CREATE VIEW lfg_signups WITH (security_barrier=true) AS SELECT inspection_read_probe(lfg_id) AS lfg_id,user_id,role_key,joined_at FROM inspection_signups_fixture",
    ] { sqlx::query(statement).execute(pool).await.expect("install isolated roster read spy"); }
}

async fn reset_read_spy(pool: &sqlx::PgPool) {
    sqlx::query("SELECT setval('inspection_roster_reads',1,false)")
        .execute(pool)
        .await
        .expect("reset fixture spy");
}

async fn read_spy_called(pool: &sqlx::PgPool) -> bool {
    sqlx::query_scalar("SELECT is_called FROM inspection_roster_reads")
        .fetch_one(pool)
        .await
        .expect("read fixture spy")
}

#[tokio::test]
async fn unknown_foreign_and_other_channel_ids_share_one_private_miss_before_roster_reads() {
    with_database(|pool| {
        Box::pin(async move {
            seed_roster(pool).await;
            seed_post(pool, "other-channel", GUILD, "5555").await;
            seed_post(pool, "other-guild", "6666", CHANNEL).await;
            let before = snapshot(pool).await;
            install_read_spy(pool).await;
            // Positive and negative oracle controls: the spy sees actual store reads,
            // including a query whose result is empty. It is not a source-text pin.
            lfg_store::list_lfg_roles(pool, POST)
                .await
                .expect("spy accepts ordinary role read");
            assert!(read_spy_called(pool).await);
            reset_read_spy(pool).await;
            lfg_store::list_lfg_signups(pool, "unknown")
                .await
                .expect("empty read still observed");
            assert!(read_spy_called(pool).await);
            reset_read_spy(pool).await;
            let mock =
                MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"8000"}))).await;
            let rt = runtime(pool.clone(), &mock, gates());
            let mut misses = Vec::new();
            for (n, id) in ["unknown", "other-guild", "other-channel"]
                .iter()
                .enumerate()
            {
                let reply = handle(
                    &rt,
                    &mock,
                    &inspect(45_000 + n as u64, id, Some(1), Some(1)),
                )
                .await;
                assert_private(&reply);
                assert!(
                    !read_spy_called(pool).await,
                    "guild and invoking channel must be checked before role OR signup reads"
                );
                assert!(
                    !reply.content.contains("Saved raid title")
                        && member_rows(&reply.content).is_empty()
                );
                let lower = reply.content.to_lowercase();
                assert!(
                    (lower.contains("not found") || lower.contains("no saved"))
                        && lower.contains("channel"),
                    "explicit private miss, not empty roster: {}",
                    reply.content
                );
                misses.push(reply.content);
            }
            assert!(
                misses.windows(2).all(|pair| pair[0] == pair[1]),
                "unknown, other-guild and other-channel IDs reveal no distinction"
            );
            let authorized = handle(&rt, &mock, &inspect(45_010, POST, Some(1), Some(1))).await;
            assert_private(&authorized);
            assert!(
                !member_rows(&authorized.content).is_empty(),
                "same-channel positive control remains readable"
            );
            assert!(
                read_spy_called(pool).await,
                "authorized roster actually reads saved data"
            );
            drop(rt);
            mock.shutdown().await;
            for statement in [
                "DROP VIEW lfg_roles",
                "DROP VIEW lfg_signups",
                "ALTER TABLE inspection_roles_fixture RENAME TO lfg_roles",
                "ALTER TABLE inspection_signups_fixture RENAME TO lfg_signups",
            ] {
                sqlx::query(statement)
                    .execute(pool)
                    .await
                    .expect("restore fixture names");
            }
            assert_eq!(
                before,
                snapshot(pool).await,
                "even hidden-ID refusals write nothing"
            );
        })
    })
    .await;
}

#[tokio::test]
async fn permissions_are_rechecked_after_a_successful_page_and_failed_ack_reads_no_roster() {
    with_database(|pool| {
        Box::pin(async move {
            seed_roster(pool).await;
            install_read_spy(pool).await;
            let mock =
                MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"8000"}))).await;
            let rt = runtime(pool.clone(), &mock, gates());
            let allowed = handle(&rt, &mock, &inspect(46_001, POST, Some(1), Some(1))).await;
            assert_private(&allowed);
            assert!(!member_rows(&allowed.content).is_empty());
            reset_read_spy(pool).await;
            let denied = handle(
                &rt,
                &mock,
                &alter(inspect(46_002, POST, Some(1), Some(2)), |v| {
                    v["member"]["permissions"] = json!("0")
                }),
            )
            .await;
            assert_private(&denied);
            assert_eq!(denied.content, two_bot_core::MANAGE_EVENTS_REQUIRED);
            assert!(
                !read_spy_called(pool).await,
                "a prior authorized page is not continuing authority"
            );
            drop(rt);
            mock.shutdown().await;
            let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
            let rt = runtime(pool.clone(), &mock, gates());
            let before = read_spy_called(pool).await;
            let _ = rt.handle(&inspect(46_003, POST, Some(1), Some(1))).await;
            assert_eq!(
                before,
                read_spy_called(pool).await,
                "failed acknowledgement cannot start a roster read"
            );
            assert!(
                mock.requests()
                    .iter()
                    .all(|r| r.path.starts_with("/api/v10/interactions/")),
                "no fallback send/edit after failed callback"
            );
            drop(rt);
            mock.shutdown().await;
        })
    })
    .await;
}

#[tokio::test]
async fn published_inspection_is_bounded_staff_only_and_discoverable_in_compiled_help() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rt = runtime(closed_pool().await, &mock, gates());
    let defs = rt
        .router
        .publish_set(&[])
        .expect("live compiled definitions");
    let value = serde_json::to_value(&defs).expect("registry JSON");
    let command = value
        .as_array()
        .expect("definitions")
        .iter()
        .find(|v| v["name"] == "lfg-inspect")
        .expect("private inspect command must be published alongside edit/list");
    assert_eq!(
        command["default_member_permissions"],
        PERM_MANAGE_EVENTS.to_string()
    );
    let options = command["options"].as_array().expect("options");
    assert_eq!(options.len(), 3, "no channel selector or member lookup");
    let id = options
        .iter()
        .find(|v| v["name"] == "id")
        .expect("saved ID option");
    assert_eq!(id["type"], 3);
    assert_eq!(id["required"], true);
    assert_eq!(id["max_length"], 128);
    for (name, max) in [("role", 20), ("page", 100)] {
        let option = options
            .iter()
            .find(|v| v["name"] == name)
            .expect("positive bounded option");
        assert_eq!(option["type"], 4);
        assert_eq!(option["required"], false);
        assert_eq!(option["min_value"], 1);
        assert_eq!(option["max_value"], max);
    }
    let help = two_bot_core::help::help_text(&defs);
    assert!(help.contains("/lfg-inspect") && help.contains("Manage Events"));
    assert!(mock.requests().is_empty());
    drop(rt);
    mock.shutdown().await;
}

#[tokio::test]
async fn control_roster_fixture_is_complete_and_observes_real_reads() {
    with_database(|pool| {
        Box::pin(async move {
            seed_roster(pool).await;
            let post = lfg_store::get_lfg(pool, GUILD, POST)
                .await
                .expect("saved post")
                .expect("post exists");
            let roles = lfg_store::list_lfg_roles(pool, POST).await.expect("roles");
            let signups = lfg_store::list_lfg_signups(pool, POST)
                .await
                .expect("signups");
            assert_eq!(roles.len(), 20);
            assert_eq!(signups.len(), 369);
            for (n, role) in roles.iter().enumerate() {
                assert_eq!(role.role_key, format!("r{n:02}"));
                assert_eq!(role.position, n);
                assert_eq!(
                    signups
                        .iter()
                        .filter(|signup| signup.role_key == role.role_key)
                        .count(),
                    filled(n)
                );
            }
            let public = lfg::lfg_content(&post, &roles, &signups);
            assert!(public.encode_utf16().count() <= 2000);
            assert!(!public.contains("RosterRole19"));
            let before = snapshot(pool).await;
            install_read_spy(pool).await;
            assert!(!read_spy_called(pool).await);
            lfg_store::list_lfg_roles(pool, POST)
                .await
                .expect("populated role read");
            assert!(read_spy_called(pool).await);
            reset_read_spy(pool).await;
            assert!(lfg_store::list_lfg_signups(pool, "unknown")
                .await
                .expect("empty signup read")
                .is_empty());
            assert!(read_spy_called(pool).await);
            for statement in [
                "DROP VIEW lfg_roles",
                "DROP VIEW lfg_signups",
                "ALTER TABLE inspection_roles_fixture RENAME TO lfg_roles",
                "ALTER TABLE inspection_signups_fixture RENAME TO lfg_signups",
            ] {
                sqlx::query(statement)
                    .execute(pool)
                    .await
                    .expect("restore fixture names");
            }
            assert_eq!(before, snapshot(pool).await);
        })
    })
    .await;
}

#[tokio::test]
async fn control_existing_preview_remains_pure_and_existing_list_still_works() {
    with_database(|pool| {
        Box::pin(async move {
            seed_post(pool, POST, GUILD, CHANNEL).await;
            let mock =
                MockRest::start(vec![], ScriptedResponse::json(200, json!({"id":"8000"}))).await;
            let rt = runtime(pool.clone(), &mock, gates());
            let list = read(&rt, &mock, pool, invocation(47_001, "lfg-list", vec![])).await;
            assert!(list.content.contains(POST) && list.content.contains("Saved raid title"));
            let preview = invocation(
                47_002,
                "lfg",
                vec![
                    json!({"type":3,"name":"title","value":"Preview control"}),
                    json!({"type":3,"name":"starts-at","value":START}),
                    json!({"type":3,"name":"roles","value":"tank:Tank:2"}),
                    json!({"type":5,"name":"preview","value":true}),
                ],
            );
            let reply = read(&rt, &mock, pool, preview).await;
            assert!(reply.content.contains("Preview control") && reply.content.contains("Tank"));
            drop(rt);
            mock.shutdown().await;
        })
    })
    .await;
}

#[tokio::test]
async fn inspection_does_not_consume_signup_cooldown_or_replace_existing_edit_and_close_paths() {
    with_database(|pool| Box::pin(async move {
        seed_post(pool,POST,GUILD,CHANNEL).await;
        sqlx::query("INSERT INTO lfg_roles (lfg_id,role_key,label,slots,position) VALUES ($1,'tank','Tank',3,0)")
            .bind(POST).execute(pool).await.expect("signup control role");
        let mock=MockRest::start(vec![],ScriptedResponse::json(200,json!({"id":"8000"}))).await;
        let rt=runtime(pool.clone(),&mock,gates());
        read(&rt,&mock,pool,inspect(48_001,POST,None,None)).await;
        let signup=alter(inspect(48_002,POST,None,None),|v| {
            v["type"]=json!(3);
            v["data"]=json!({"custom_id":lfg::lfg_custom_id(POST),"component_type":3,"values":["tank"]});
        });
        let reply=handle(&rt,&mock,&signup).await;
        assert!(reply.handled);
        assert!(!reply.content.to_lowercase().contains("wait 5 seconds"),"inspection must not consume signup admission");
        let saved=lfg_store::list_lfg_signups(pool,POST).await.expect("saved signups after control");
        assert_eq!(saved.len(),1); assert_eq!(saved[0].user_id,"3333");
        let before=mock.requests().len();
        let roster=read(&rt,&mock,pool,inspect(48_003,POST,Some(1),Some(1))).await;
        assert!(roster.content.contains("3333"),"saved signup remains visible after ordinary apply");
        assert!(mock.requests()[before..].iter().all(|r|!r.path.contains("/channels/")));
        let edit=invocation(48_004,"lfg-edit",vec![json!({"type":3,"name":"id","value":POST}),json!({"type":3,"name":"title","value":"Edited control"})]);
        assert!(handle(&rt,&mock,&edit).await.handled);
        let post=lfg_store::get_lfg(pool,GUILD,POST).await.expect("edited post").expect("exists");
        assert_eq!(post.title,"Edited control");
        let close=invocation(48_005,"lfg-close",vec![json!({"type":3,"name":"id","value":POST})]);
        assert!(handle(&rt,&mock,&close).await.handled);
        assert_eq!(lfg_store::get_lfg(pool,GUILD,POST).await.expect("closed post").expect("exists").status,lfg::LfgStatus::Closed);
        assert_eq!(lfg_store::list_lfg_signups(pool,POST).await.expect("preserved signup"),saved);
        drop(rt); mock.shutdown().await;
    })).await;
}
