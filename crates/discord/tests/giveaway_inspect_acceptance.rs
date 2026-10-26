//! Protected failing-first contract: saved giveaway inspection and static entry rules.
//! Builders consume this file unchanged; test defects return to its author.
//! `inspect id:<saved-id>` is the new explicit staff mode. No eligibility probe,
//! cross-channel resolver, background job, or new permission is part of this slice.
//! All SQL is confined to a TestDatabase fixture; Discord is loopback-only.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use futures_util::{future::BoxFuture, FutureExt};
use serde_json::{json, Value};
use std::panic::AssertUnwindSafe;
use twilight_model::application::interaction::Interaction;
use two_bot_core::{commands::PERM_MANAGE_GUILD, ClassifierConfig, InteractionRouter, RouterGates};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "2222";
const CHANNEL: &str = "4444";
const ID: &str = "7001";
const DEADLINE: i64 = 4_071_006_245;
const NONCE: &str = "PRIVATE_DRAW_NONCE_DO_NOT_RENDER";
const CLAIM: &str = "PRIVATE_CLAIM_DO_NOT_RENDER";
const ENTRANT: &str = "12345678987654321";

fn gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(2222),
        automations: true,
        announcements: true,
        scorecard: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn subcommand(name: &str, options: Value) -> Value {
    json!([{"name": name, "type": 1, "options": options}])
}

fn id_options(name: &str, id: &str) -> Value {
    subcommand(name, json!([{"name": "id", "type": 3, "value": id}]))
}

fn slash(id: u64, options: Value) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
        "member": {"user": {"id": "3333", "username": "staff", "discriminator": "0"},
            "roles": [], "deaf": false, "mute": false, "flags": 0,
            "permissions": PERM_MANAGE_GUILD.to_string()},
        "token": "giveaway-inspect-fixture", "version": 1, "entitlements": [],
        "authorizing_integration_owners": {},
        "data": {"id": "1", "name": "giveaway", "type": 1, "options": options}
    }))
    .expect("synthetic giveaway interaction")
}

fn change(event: Interaction, key: &str, value: Value) -> Interaction {
    let mut wire = serde_json::to_value(event).expect("serialize fixture");
    wire[key] = value;
    serde_json::from_value(wire).expect("modified fixture")
}

fn runtime(pool: sqlx::PgPool, mock: &MockRest, gates: RouterGates) -> InteractionRuntime {
    InteractionRuntime::with_router(
        InteractionRouter::production_router(gates, false),
        pool,
        ActionExecutor::with_proxy(
            "giveaway-fixture-not-a-credential".into(),
            Some(mock.origin()),
        )
        .expect("loopback executor"),
        0,
        ClassifierConfig::default(),
    )
}

async fn mock() -> MockRest {
    MockRest::with_responder(|request| {
        if request.path.contains("/interactions/") {
            ScriptedResponse::status(204)
        } else if request.method == "GET" {
            // Any such read during inspection is a contract failure, even if
            // the implementation swallows the failure and still renders a reply.
            ScriptedResponse::json(500, json!({"message": "PRIVATE_PROVIDER_DETAIL"}))
        } else {
            ScriptedResponse::json(200, json!({"id": "8001"}))
        }
    })
    .await
}

async fn with_database<F>(case: F)
where
    F: for<'a> FnOnce(&'a sqlx::PgPool) -> BoxFuture<'a, ()>,
{
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database");
    let result = AssertUnwindSafe(case(db.pool())).catch_unwind().await;
    db.close().await.expect("explicit fixture teardown");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn seed(pool: &sqlx::PgPool, guild: &str, channel: &str, status: &str, requirements: Value) {
    sqlx::query(
        "INSERT INTO giveaways
         (guild_id,id,prize,channel_id,message_id,created_by,winners_count,ends_at,status,
          requirements_json,max_entries,draw_nonce,claim_token,created_at,updated_at)
         VALUES ($1,$2,'Saved reward',$3,'8001','3333',2,to_timestamp($4),$5,$6,100,$7,$8,
                 '2020-01-01T00:00:00Z','2020-01-01T00:00:00Z')",
    )
    .bind(guild)
    .bind(ID)
    .bind(channel)
    .bind(DEADLINE as f64)
    .bind(status)
    .bind(requirements)
    .bind(NONCE)
    .bind(CLAIM)
    .execute(pool)
    .await
    .expect("seed saved giveaway");
}

async fn entries(pool: &sqlx::PgPool, guild: &str) {
    for user in [ENTRANT, "12345678987654322", "12345678987654323"] {
        sqlx::query(
            "INSERT INTO giveaway_entries (guild_id,giveaway_id,user_id,weight,entered_at)
                     VALUES ($1,$2,$3,1,'2020-02-03T04:05:06Z')",
        )
        .bind(guild)
        .bind(ID)
        .bind(user)
        .execute(pool)
        .await
        .expect("seed private entrant");
    }
}

async fn snapshot(pool: &sqlx::PgPool) -> Value {
    // Include MVCC row versions: even UPDATE-to-the-same-value is an effect.
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
         'giveaways',(SELECT jsonb_agg(to_jsonb(g)||jsonb_build_object('_xmin',g.xmin::text) ORDER BY g.guild_id,g.id) FROM giveaways g),
         'entries',(SELECT jsonb_agg(to_jsonb(e)||jsonb_build_object('_xmin',e.xmin::text) ORDER BY e.guild_id,e.giveaway_id,e.user_id) FROM giveaway_entries e),
         'draws',(SELECT jsonb_agg(to_jsonb(d)||jsonb_build_object('_xmin',d.xmin::text) ORDER BY d.guild_id,d.giveaway_id,d.draw_no) FROM giveaway_draws d),
         'audit',(SELECT jsonb_agg(to_jsonb(a)||jsonb_build_object('_xmin',a.xmin::text) ORDER BY a.id) FROM automation_audit_log a))",
    ).fetch_one(pool).await.expect("snapshot read-only boundary")
}

fn body(request: &RestRequest) -> Value {
    serde_json::from_slice(&request.body).expect("Discord JSON")
}

fn content(requests: &[RestRequest]) -> String {
    requests
        .iter()
        .rev()
        .find_map(|request| {
            let payload = body(request);
            payload
                .get("content")
                .or_else(|| payload.get("data")?.get("content"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

fn assert_safe_text(text: &str) {
    assert!(
        !text.is_empty(),
        "inspection needs an explicit private result"
    );
    assert!(
        text.encode_utf16().count() <= 2000,
        "Discord UTF-16 framing exceeded"
    );
    for private in [
        NONCE,
        CLAIM,
        ENTRANT,
        "2020-02-03",
        "PRIVATE_PROVIDER_DETAIL",
        "sqlx::",
        "SELECT ",
        "postgres://",
    ] {
        assert!(!text.contains(private), "inspection leaked {private}");
    }
}

fn assert_private_only(requests: &[RestRequest], interaction: u64) -> String {
    assert!(
        !requests.is_empty() && requests.len() <= 2,
        "one private ack and optional edit only"
    );
    for (index, request) in requests.iter().enumerate() {
        let payload = body(request);
        if index == 0 {
            assert_eq!(request.method, "POST");
            assert!(request.path.ends_with(&format!(
                "/interactions/{interaction}/giveaway-inspect-fixture/callback"
            )));
            assert_eq!(
                payload["data"]["flags"].as_u64().unwrap_or(0) & 64,
                64,
                "staff response must be ephemeral"
            );
            if payload["type"] == 4 {
                assert_eq!(payload["data"]["allowed_mentions"]["parse"], json!([]));
            } else {
                assert_eq!(payload["type"], 5, "private response or private defer");
            }
        } else {
            assert_eq!(request.method, "PATCH");
            assert!(request
                .path
                .ends_with("/webhooks/1111/giveaway-inspect-fixture/messages/@original"));
            assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
        }
        assert!(
            !request.path.contains("/channels/") && !request.path.contains("/guilds/"),
            "inspection must not send/edit publicly or read member/channel profiles"
        );
    }
    let text = content(requests);
    assert_safe_text(&text);
    text
}

async fn inspect(pool: &sqlx::PgPool, options: Value, id: u64) -> String {
    let mock = mock().await;
    let rt = runtime(pool.clone(), &mock, gates());
    assert!(rt
        .handle(&slash(id, options))
        .await
        .expect("runtime response"));
    let requests = mock.requests();
    drop(rt);
    mock.shutdown().await;
    assert_private_only(&requests, id)
}

fn assert_detail(text: &str, status: &str, count: u64) {
    assert!(
        text.contains(ID) && text.contains("Saved reward"),
        "saved identity and prize missing: {text}"
    );
    assert!(
        text.to_lowercase().contains(status),
        "saved lifecycle missing: {text}"
    );
    assert!(
        text.contains(&format!("<t:{DEADLINE}:")),
        "deadline must be Discord-localized: {text}"
    );
    assert!(
        text.contains(CHANNEL) && text.contains("8001"),
        "authorized saved reference missing: {text}"
    );
    assert_labeled_number(text, "entr", count);
    assert_labeled_number(text, "winner", 2);
    assert!(
        text.to_lowercase().contains("saved"),
        "do not imply a live Discord read: {text}"
    );
}

fn assert_labeled_number(text: &str, label: &str, number: u64) {
    assert!(
        text.lines().any(|line| {
            line.to_lowercase().contains(label)
                && line
                    .split(|c: char| !c.is_ascii_digit())
                    .any(|n| n == number.to_string())
        }),
        "missing labeled {label} count {number}: {text}"
    );
}

fn requirements_line(text: &str) -> String {
    text.lines()
        .find(|line| line.to_lowercase().contains("requirements:"))
        .expect("compact labeled static requirements summary")
        .trim()
        .to_owned()
}

fn assert_unconfirmed(text: &str) {
    let lower = text.to_lowercase();
    assert!(
        lower.contains("announcement")
            && ["unconfirmed", "not confirmed", "pending", "not yet"]
                .iter()
                .any(|word| lower.contains(word)),
        "saved draw is not proof of a public announcement: {text}"
    );
}

#[test]
fn registry_offers_explicit_inspect_without_losing_existing_verbs() {
    let definition = two_bot_core::feature_commands::giveaway_command();
    let wire = serde_json::to_value(two_bot_discord::interactions::command_to_twilight(
        &definition,
    ))
    .expect("published command wire");
    assert_eq!(wire["default_member_permissions"], "32");
    let options = wire["options"].as_array().expect("slash subcommands");
    for name in [
        "create", "list", "cancel", "end", "pause", "resume", "extend", "reroll",
    ] {
        assert!(
            options.iter().any(|option| option["name"] == name),
            "lost existing {name} control"
        );
    }
    let inspect = options
        .iter()
        .find(|option| option["name"] == "inspect")
        .expect("explicit saved-detail mode is absent");
    assert_eq!(inspect["type"], 1);
    let args = inspect["options"].as_array().expect("inspect arguments");
    assert_eq!(
        args.len(),
        1,
        "inspection is id-only, not an eligibility or cross-channel query"
    );
    assert_eq!(args[0]["name"], "id");
    assert_eq!(args[0]["type"], 3);
    assert_eq!(args[0]["required"], true);
    assert!(args[0]["max_length"].as_u64().is_some_and(|max| max <= 64));
}

#[tokio::test]
async fn saved_detail_covers_lifecycle_counts_and_delivery_truth_without_effects() {
    with_database(|pool| Box::pin(async move {
        seed(pool, GUILD, CHANNEL, "open", json!({})).await;
        entries(pool, GUILD).await;
        // A same-ID foreign row and roster must not contaminate any count or detail.
        seed(pool, "9999", CHANNEL, "cancelled", json!({})).await;
        entries(pool, "9999").await;
        for (index, status) in ["open", "paused", "drawing", "drawn", "cancelled"].into_iter().enumerate() {
            sqlx::query("UPDATE giveaways SET status=$1 WHERE guild_id=$2 AND id=$3")
                .bind(status).bind(GUILD).bind(ID).execute(pool).await.expect("fixture lifecycle");
            let before = snapshot(pool).await;
            let text = inspect(pool, id_options("inspect", ID), 9000 + index as u64).await;
            assert_eq!(snapshot(pool).await, before, "inspection changed rows, leases, entries or audit");
            assert_detail(&text, status, 3);
            assert!(requirements_line(&text).to_lowercase().contains("everyone"));
            if status == "drawn" { assert_unconfirmed(&text); }
        }
        sqlx::query("UPDATE giveaways SET status='drawn',announced_at=now() WHERE guild_id=$1 AND id=$2")
            .bind(GUILD).bind(ID).execute(pool).await.expect("confirmed announcement fixture");
        let before = snapshot(pool).await;
        let text = inspect(pool, id_options("inspect", ID), 9010).await;
        assert_eq!(snapshot(pool).await, before);
        assert_detail(&text, "drawn", 3);
        let lower = text.to_lowercase();
        assert!(lower.contains("announcement") && (lower.contains("confirmed") || lower.contains("announced")));
        assert!(!lower.contains("unconfirmed") && !lower.contains("not confirmed"));
    })).await;
}

#[tokio::test]
async fn absent_draw_receipt_does_not_turn_a_drawn_status_into_confirmed_delivery() {
    with_database(|pool| {
        Box::pin(async move {
            seed(pool, GUILD, CHANNEL, "drawn", json!({})).await;
            let text = inspect(pool, id_options("inspect", ID), 9020).await;
            assert_unconfirmed(&text);
            assert_labeled_number(&text, "entr", 0);
            // The requested winner count remains visible, but no winner roster is disclosed.
            assert_labeled_number(&text, "winner", 2);
        })
    })
    .await;
}

#[tokio::test]
async fn unknown_foreign_and_other_channel_ids_share_one_private_miss_before_counts() {
    with_database(|pool| {
        Box::pin(async move {
            seed(pool, GUILD, "5555", "open", json!({})).await;
            seed(pool, "9999", CHANNEL, "open", json!({})).await;
            entries(pool, GUILD).await;
            // A premature supplemental read cannot accidentally succeed. These
            // fixture-only renames leave get_giveaway available but count/draw reads fail.
            sqlx::query("ALTER TABLE giveaway_entries RENAME TO inspect_blocked_entries")
                .execute(pool)
                .await
                .expect("count-read tripwire");
            sqlx::query("ALTER TABLE giveaway_draws RENAME TO inspect_blocked_draws")
                .execute(pool)
                .await
                .expect("draw-read tripwire");
            let hidden = inspect(pool, id_options("inspect", ID), 9030).await;
            let unknown = inspect(pool, id_options("inspect", "7999"), 9031).await;
            // Renaming preserves the foreign key. Remove the fixture's child
            // rows before its parent so the foreign-only case reaches its oracle.
            sqlx::query("DELETE FROM inspect_blocked_entries WHERE guild_id=$1 AND giveaway_id=$2")
                .bind(GUILD)
                .bind(ID)
                .execute(pool)
                .await
                .expect("remove hidden fixture entries");
            sqlx::query("DELETE FROM giveaways WHERE guild_id=$1 AND id=$2")
                .bind(GUILD)
                .bind(ID)
                .execute(pool)
                .await
                .expect("retain foreign-only row");
            let foreign = inspect(pool, id_options("inspect", ID), 9032).await;
            assert_eq!(hidden, unknown, "other-channel existence leaked");
            assert_eq!(foreign, unknown, "foreign existence leaked");
            let lower = unknown.to_lowercase();
            assert!(
                !lower.contains("invalid") && !lower.contains("unavailable"),
                "valid private miss is not malformed or store failure: {unknown}"
            );
            assert!(["unknown", "not found", "no giveaway"]
                .iter()
                .any(|word| lower.contains(word)));
            for text in [hidden, unknown, foreign] {
                assert!(
                    !text.contains("Saved reward")
                        && !text.contains("5555")
                        && !text.contains("8001")
                );
            }
        })
    })
    .await;
}

#[tokio::test]
async fn malformed_inspect_options_are_rejected_without_supplemental_work() {
    with_database(|pool| Box::pin(async move {
        seed(pool, GUILD, CHANNEL, "open", json!({})).await;
        let before = snapshot(pool).await;
        let id = json!({"name":"id","type":3,"value":ID});
        let cases = vec![
            subcommand("inspect", json!([])),
            subcommand("inspect", json!([{"name":"id","type":4,"value":7001}])),
            subcommand("inspect", json!([id.clone(), id.clone()])),
            subcommand("inspect", json!([id, {"name":"user","type":6,"value":"3333"}])),
            id_options("inspect", ""), id_options("inspect", &"7".repeat(65)),
            json!([{"name":"inspect","type":3,"value":ID}]),
            json!([{"name":"inspect","type":1,"options":[]},{"name":"list","type":1,"options":[]}]),
        ];
        for (index, options) in cases.into_iter().enumerate() {
            let text = inspect(pool, options, 9040 + index as u64).await;
            assert!(text.to_lowercase().contains("invalid"), "malformed inspect accepted: {text}");
            assert_eq!(snapshot(pool).await, before);
        }
    })).await;
}

#[tokio::test]
async fn unavailable_store_is_not_reported_as_empty_or_unknown_or_invalid() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_local")
        .expect("synthetic guarded target");
    // No connection is attempted: PoolClosed is the unavailable dependency.
    pool.close().await;
    let text = inspect(&pool, id_options("inspect", ID), 9060).await;
    assert!(
        text.to_lowercase().contains("unavailable"),
        "valid inspect needs a bounded unavailable result: {text}"
    );
    assert!(!text.to_lowercase().contains("invalid") && !text.contains("No open"));
}

#[tokio::test]
async fn failed_supplemental_read_must_not_invent_zero_entries() {
    with_database(|pool| {
        Box::pin(async move {
            seed(pool, GUILD, CHANNEL, "open", json!({})).await;
            entries(pool, GUILD).await;
            sqlx::query("ALTER TABLE giveaway_entries RENAME TO inspect_unavailable_entries")
                .execute(pool)
                .await
                .expect("count-read failure fixture");
            let text = inspect(pool, id_options("inspect", ID), 9061).await;
            let lower = text.to_lowercase();
            assert!(
                lower.contains("unavailable") || lower.contains("unknown"),
                "count read failure must be explicit: {text}"
            );
            assert!(!lower.contains("invalid"));
            assert!(
                !text.lines().any(|line| line.to_lowercase().contains("entr")
                    && line.split(|c: char| !c.is_ascii_digit()).any(|n| n == "0")),
                "unknown is not zero"
            );
        })
    })
    .await;
}

#[tokio::test]
async fn authority_feature_and_guild_controls_still_fence_inspect() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_local")
        .expect("synthetic target");
    pool.close().await;
    let event = slash(9070, id_options("inspect", ID));
    let mut member =
        serde_json::to_value(event.member.as_ref().expect("fixture member")).expect("member wire");
    member["permissions"] = json!("0");
    let unprivileged = change(event.clone(), "member", member);
    let mut disabled = gates();
    disabled.automations = false;
    disabled.announcements = false;
    let mut unconfigured = gates();
    unconfigured.configured_guild = None;
    for (gates, event, expected_reply) in [
        (gates(), unprivileged, true),
        (disabled, event.clone(), true),
        (unconfigured, event.clone(), false),
        (
            gates(),
            change(event.clone(), "guild_id", json!("9999")),
            false,
        ),
        (gates(), change(event, "guild_id", Value::Null), false),
    ] {
        let mock = mock().await;
        let rt = runtime(pool.clone(), &mock, gates);
        let handled = rt.handle(&event).await.expect("fenced interaction");
        let requests = mock.requests();
        drop(rt);
        mock.shutdown().await;
        assert_eq!(
            handled, expected_reply,
            "preserve configured-guild refusal versus foreign-guild silence"
        );
        assert_eq!(
            requests.len(),
            usize::from(expected_reply),
            "one private refusal, never a defer or saved read"
        );
        if expected_reply {
            let text = assert_private_only(&requests, 9070);
            let lower = text.to_lowercase();
            assert!(
                !lower.contains("unavailable") && !lower.contains("invalid"),
                "invocation guard lost: {text}"
            );
        }
    }
}

async fn requirements_case(
    options: Value,
    expected_roles: &[&str],
    ages: &[&str],
    unrestricted: bool,
) {
    let expected_roles: Vec<String> = expected_roles
        .iter()
        .map(|role| (*role).to_owned())
        .collect();
    let ages: Vec<String> = ages.iter().map(|age| (*age).to_owned()).collect();
    with_database(move |pool| {
        Box::pin(async move {
            let mut create = vec![
                json!({"name":"prize","type":3,"value":"Saved reward"}),
                json!({"name":"duration","type":4,"value":60}),
                json!({"name":"winners","type":4,"value":2}),
            ];
            create.extend(
                options
                    .as_array()
                    .expect("extra create options")
                    .iter()
                    .cloned(),
            );
            let mock = mock().await;
            let rt = runtime(pool.clone(), &mock, gates());
            assert!(rt
                .handle(&slash(7001, subcommand("create", json!(create))))
                .await
                .expect("create control"));
            let created = mock.requests();
            let public = created
                .iter()
                .find(|request| {
                    request.method == "POST" && request.path.ends_with("/channels/4444/messages")
                })
                .expect("existing create must still publish its entry post");
            let payload = body(public);
            assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
            let text = payload["content"].as_str().expect("entry post content");
            let summary = requirements_line(text);
            assert!(
                payload["components"].to_string().contains("gw:7001"),
                "entry button must remain usable"
            );
            assert_safe_text(text);
            let before = snapshot(pool).await;
            let start = created.len();
            assert!(rt
                .handle(&slash(9080, id_options("inspect", ID)))
                .await
                .expect("inspect created giveaway"));
            let requests = mock.requests();
            drop(rt);
            mock.shutdown().await;
            let private = assert_private_only(&requests[start..], 9080);
            assert_eq!(snapshot(pool).await, before);
            assert_eq!(
                requirements_line(&private),
                summary,
                "entry and saved detail must expose one consistent static summary"
            );
            let lower = summary.to_lowercase();
            assert_eq!(lower.contains("everyone"), unrestricted);
            for role in expected_roles {
                assert!(
                    summary.contains(role.as_str()),
                    "missing saved role identifier"
                );
                assert!(
                    !summary.contains(&format!("<@&{role}>")),
                    "role identifiers must be inert"
                );
            }
            for age in ages {
                assert!(
                    lower.contains(age.as_str()),
                    "missing minimum age {age}: {summary}"
                );
            }
            for promise in [
                "you qualify",
                "you are eligible",
                "guaranteed",
                "you will win",
            ] {
                assert!(
                    !lower.contains(promise),
                    "static rules are not a member eligibility or winning promise"
                );
            }
        })
    })
    .await;
}

#[tokio::test]
async fn unrestricted_entry_post_and_private_detail_share_everyone_summary() {
    requirements_case(json!([]), &[], &[], true).await;
}

#[tokio::test]
async fn role_requirement_is_inert_and_shared_before_entry() {
    requirements_case(
        json!([{"name":"required-role","type":8,"value":"6666"}]),
        &["6666"],
        &[],
        false,
    )
    .await;
}

#[tokio::test]
async fn account_age_requirement_is_visible_before_entry() {
    requirements_case(
        json!([{"name":"min-account-age-days","type":4,"value":7}]),
        &[],
        &["account", "7", "day"],
        false,
    )
    .await;
}

#[tokio::test]
async fn server_age_requirement_is_visible_before_entry() {
    requirements_case(
        json!([{"name":"min-server-age-days","type":4,"value":3}]),
        &[],
        &["server", "3", "day"],
        false,
    )
    .await;
}

#[tokio::test]
async fn combined_static_rules_survive_public_create_and_private_inspection() {
    requirements_case(
        json!([
            {"name":"required-role","type":8,"value":"6666"},
            {"name":"min-account-age-days","type":4,"value":7},
            {"name":"min-server-age-days","type":4,"value":3}
        ]),
        &["6666"],
        &["account", "7", "server", "3", "day"],
        false,
    )
    .await;
}

#[tokio::test]
async fn hostile_unicode_prize_cannot_overflow_or_consume_required_framing() {
    with_database(|pool| {
        Box::pin(async move {
            seed(
                pool,
                GUILD,
                CHANNEL,
                "open",
                json!({"roles":["6666"],"min_account_age_days":7,"min_guild_age_days":3}),
            )
            .await;
            // Oversized saved data models drift/import corruption, not a larger create limit.
            let prize = format!(
                "@everyone <@3333> <@&6666> ```\n{} END_OF_HOSTILE_PRIZE",
                "🦀".repeat(2200)
            );
            sqlx::query("UPDATE giveaways SET prize=$1 WHERE guild_id=$2 AND id=$3")
                .bind(prize)
                .bind(GUILD)
                .bind(ID)
                .execute(pool)
                .await
                .expect("hostile saved fixture");
            let before = snapshot(pool).await;
            let text = inspect(pool, id_options("inspect", ID), 9090).await;
            assert_eq!(snapshot(pool).await, before);
            assert!(
                !text.contains("END_OF_HOSTILE_PRIZE"),
                "saved prize must be bounded, not copied wholesale"
            );
            assert!(text.contains(ID) && text.contains(&format!("<t:{DEADLINE}:")));
            assert_labeled_number(&text, "entr", 0);
            assert_labeled_number(&text, "winner", 2);
            let summary = requirements_line(&text);
            assert!(summary.contains("6666") && summary.contains('7') && summary.contains('3'));
        })
    })
    .await;
}

#[tokio::test]
async fn unconfirmed_entry_message_never_becomes_a_fake_jump_link() {
    with_database(|pool| {
        Box::pin(async move {
            seed(pool, GUILD, CHANNEL, "open", json!({})).await;
            sqlx::query("UPDATE giveaways SET message_id='pending' WHERE guild_id=$1 AND id=$2")
                .bind(GUILD)
                .bind(ID)
                .execute(pool)
                .await
                .expect("unconfirmed entry delivery");
            let text = inspect(pool, id_options("inspect", ID), 9091).await;
            let lower = text.to_lowercase();
            assert!(
                lower.contains("unconfirmed")
                    || lower.contains("not attached")
                    || lower.contains("pending")
            );
            assert!(!text.contains("/pending") && !text.contains("8001"));
        })
    })
    .await;
}

#[tokio::test]
async fn repeated_inspection_consumes_no_entry_budget_and_default_list_stays_live() {
    with_database(|pool| Box::pin(async move {
        seed(pool, GUILD, CHANNEL, "open", json!({})).await;
        let mock = mock().await;
        let rt = runtime(pool.clone(), &mock, gates());
        let before = snapshot(pool).await;
        let mut cursor = 0;
        for id in 9100..9112 {
            assert!(rt.handle(&slash(id, id_options("inspect", ID))).await.expect("repeat inspection"));
            let requests = mock.requests();
            let text = assert_private_only(&requests[cursor..], id);
            assert_detail(&text, "open", 0);
            cursor = requests.len();
        }
        assert_eq!(snapshot(pool).await, before, "read mode applied a mutation or consumed admission state");
        assert!(rt.handle(&slash(9120, subcommand("list", json!([])))).await.expect("list control"));
        assert!(content(&mock.requests()[cursor..]).contains(ID), "default active-list control lost");
        // The seed holds a draw-ticker lease so the inspections above prove the
        // claim capability is neither rendered nor disturbed. Entry admission
        // is Closed while that lease is held, so release it before the entry
        // control to model the production-shaped open row the control targets.
        sqlx::query("UPDATE giveaways SET claim_token=NULL, claimed_at=NULL WHERE guild_id=$1 AND id=$2")
            .bind(GUILD).bind(ID).execute(pool).await.expect("release seeded draw lease before entry control");
        let mut button = serde_json::to_value(slash(9130, json!([]))).expect("button base");
        button["type"] = json!(3);
        button["data"] = json!({"custom_id":"gw:7001","component_type":2});
        button["member"]["permissions"] = json!("0");
        assert!(rt.handle(&serde_json::from_value(button).expect("entry button")).await.expect("entry control"));
        let admitted: i64 = sqlx::query_scalar("SELECT count(*) FROM giveaway_entries WHERE guild_id=$1 AND giveaway_id=$2 AND user_id='3333'")
            .bind(GUILD).bind(ID).fetch_one(pool).await.expect("entry admission witness");
        drop(rt);
        mock.shutdown().await;
        assert_eq!(admitted, 1, "inspection must not consume the member's entry budget or disable entry");
    })).await;
}

#[tokio::test]
async fn unchanged_store_controls_scope_saved_get_and_count_to_the_guild() {
    with_database(|pool| {
        Box::pin(async move {
            seed(pool, GUILD, CHANNEL, "open", json!({})).await;
            seed(pool, "9999", "5555", "cancelled", json!({})).await;
            entries(pool, GUILD).await;
            use two_bot_core::giveaway_store::{entry_count, get_giveaway};
            let visible = get_giveaway(pool, GUILD, ID)
                .await
                .expect("scoped get")
                .expect("saved row");
            assert_eq!(visible.channel_id, CHANNEL);
            assert_eq!(visible.status, "open");
            assert_eq!(entry_count(pool, GUILD, ID).await.expect("scoped count"), 3);
            assert_eq!(
                entry_count(pool, "9999", ID)
                    .await
                    .expect("other guild count"),
                0
            );
            assert!(get_giveaway(pool, "8888", ID)
                .await
                .expect("foreign miss")
                .is_none());
        })
    })
    .await;
}

#[tokio::test]
async fn unchanged_create_entry_list_and_lifecycle_controls_remain_operational() {
    with_database(|pool| {
        Box::pin(async move {
            let mock = mock().await;
            let rt = runtime(pool.clone(), &mock, gates());
            let create = subcommand(
                "create",
                json!([
                    {"name":"prize","type":3,"value":"Legacy control reward"},
                    {"name":"duration","type":4,"value":60},
                    {"name":"winners","type":4,"value":2}
                ]),
            );
            assert!(rt
                .handle(&slash(7201, create.clone()))
                .await
                .expect("create control"));
            assert!(rt
                .handle(&slash(
                    7202,
                    subcommand(
                        "create",
                        json!([
                            {"name":"prize","type":3,"value":"Invalid control reward"},
                            {"name":"duration","type":4,"value":60},
                            {"name":"winners","type":4,"value":0}
                        ])
                    )
                ))
                .await
                .expect("invalid create control"));
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM giveaways")
                .fetch_one(pool)
                .await
                .expect("create witness");
            assert_eq!(count, 1, "invalid create must not persist");
            let cursor = mock.requests().len();
            assert!(rt
                .handle(&slash(7210, subcommand("list", json!([]))))
                .await
                .expect("list control"));
            assert!(content(&mock.requests()[cursor..]).contains("7201"));
            let mut button = serde_json::to_value(slash(7211, json!([]))).expect("button base");
            button["type"] = json!(3);
            button["data"] = json!({"custom_id":"gw:7201","component_type":2});
            button["member"]["permissions"] = json!("0");
            assert!(rt
                .handle(&serde_json::from_value(button).expect("entry fixture"))
                .await
                .expect("entry control"));
            assert_eq!(
                two_bot_core::giveaway_store::entry_count(pool, GUILD, "7201")
                    .await
                    .expect("entry count"),
                1
            );
            let before = snapshot(pool).await;
            for (id, name) in [(7212, "end"), (7213, "reroll")] {
                let cursor = mock.requests().len();
                assert!(rt
                    .handle(&slash(id, id_options(name, "7999")))
                    .await
                    .expect("unknown lifecycle control"));
                assert!(content(&mock.requests()[cursor..])
                    .to_lowercase()
                    .contains("unknown"));
            }
            assert_eq!(
                snapshot(pool).await,
                before,
                "unknown lifecycle target must not apply effects"
            );
            // The existing empty draw is a supported result, without member/profile reads.
            assert!(rt
                .handle(&slash(7203, create))
                .await
                .expect("second create control"));
            assert!(rt
                .handle(&slash(7214, id_options("end", "7203")))
                .await
                .expect("end control"));
            let status: String =
                sqlx::query_scalar("SELECT status FROM giveaways WHERE guild_id=$1 AND id='7203'")
                    .bind(GUILD)
                    .fetch_one(pool)
                    .await
                    .expect("draw status");
            assert_eq!(status, "drawn");
            assert!(rt
                .handle(&slash(7215, id_options("reroll", "7203")))
                .await
                .expect("reroll control"));
            let rounds: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM giveaway_draws WHERE guild_id=$1 AND giveaway_id='7203'",
            )
            .bind(GUILD)
            .fetch_one(pool)
            .await
            .expect("draw rounds");
            drop(rt);
            mock.shutdown().await;
            assert_eq!(
                rounds, 2,
                "end and reroll remain independent persisted rounds"
            );
        })
    })
    .await;
}
