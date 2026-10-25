//! Protected tests-only private giveaway preview contract.
//! Run with --features db. No live Discord; SQL tripwires never speak Postgres.
//! Builder: preserve this file; implement the combined runtime/registry/docs PR.
//! The baseline adapter below lets missing behavior fail assertions, not compile.

#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool, Row};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use time::OffsetDateTime;
use twilight_model::application::interaction::Interaction;
use two_bot_core::{commands::PERM_MANAGE_GUILD, ClassifierConfig, InteractionRouter, RouterGates};
use two_bot_discord::{
    command_to_twilight, giveaway::GiveawayRuntime, interactions::InteractionRuntime,
    ActionExecutor,
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1550102030405060701";
const CHANNEL: &str = "1550102030405060702";
const ROLE: &str = "1550102030405060731";
const ACTOR: &str = "1550102030405060711";
const NOW: i64 = 1_800_000_000;
const COMPLETION: &str = "/api/v10/webhooks/1111/preview-fixture/messages/@original";

type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;
type Nonce = Arc<dyn Fn() -> String + Send + Sync>;

// Required deterministic seam: an inherent method with this exact signature
// shadows the baseline adapter. Production defaults remain wall clock/secure RNG.
// Positive create controls require the injected nonce to reach the real store;
// a preview must never invoke it. The explicit baseline control keeps this trait
// live after the builder supplies the inherent method (clippy -D warnings).
trait BaselineCreateSources {
    fn with_create_sources(self, clock: Clock, nonce: Nonce) -> Self;
}
impl BaselineCreateSources for GiveawayRuntime {
    fn with_create_sources(self, _clock: Clock, _nonce: Nonce) -> Self {
        self
    }
}

fn router(enabled: bool) -> InteractionRouter {
    InteractionRouter::new(RouterGates {
        configured_guild: Some(GUILD.parse().expect("fixture guild")),
        automations: enabled,
        announcements: true,
        ..RouterGates::default()
    })
}

fn option(name: &str, kind: u8, value: Value) -> Value {
    json!({"name": name, "type": kind, "value": value})
}
fn flag(value: bool) -> Value {
    option("preview", 5, json!(value))
}
fn options() -> Vec<Value> {
    vec![
        option("prize", 3, json!("  Friday prize  ")),
        option("duration", 4, json!(60)),
        option("winners", 4, json!(2)),
        option("required-role", 8, json!(ROLE)),
        option("min-account-age-days", 4, json!(7)),
        option("min-server-age-days", 4, json!(3)),
    ]
}
fn invocation(id: u64, options: Vec<Value>) -> Interaction {
    serde_json::from_value(json!({
        "application_id": "1111", "authorizing_integration_owners": {"0": GUILD},
        "id": id.to_string(), "token": "preview-fixture", "type": 2, "version": 1,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
        "member": {"permissions": PERM_MANAGE_GUILD.to_string(), "roles": [],
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": ACTOR, "username": "staff", "discriminator": "0"}},
        "data": {"id": "4444", "name": "giveaway", "type": 1,
            "options": [{"name": "create", "type": 1, "options": options}]}
    }))
    .expect("valid Discord wire fixture")
}
fn replace(options: &mut [Value], name: &str, kind: u8, value: Value) {
    *options
        .iter_mut()
        .find(|v| v["name"] == name)
        .expect("fixture option") = option(name, kind, value);
}
fn body(request: &RestRequest) -> Value {
    serde_json::from_slice(&request.body).expect("outbound JSON")
}
async fn rest() -> MockRest {
    MockRest::with_responder(
        |request| match (request.method.as_str(), request.path.as_str()) {
            ("POST", path) if path.ends_with("/callback") => ScriptedResponse::status(204),
            ("PATCH", COMPLETION) => ScriptedResponse::json(200, json!({"id": "5900"})),
            ("POST", path) if path == format!("/api/v10/channels/{CHANNEL}/messages") => {
                ScriptedResponse::json(200, json!({"id": "5000"}))
            }
            _ => ScriptedResponse::status(404),
        },
    )
    .await
}
fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("mock-token-not-a-credential".into(), Some(mock.origin()))
        .expect("loopback executor")
}
fn runtime(pool: &PgPool, mock: &MockRest, enabled: bool) -> InteractionRuntime {
    InteractionRuntime::with_router(
        router(enabled),
        pool.clone(),
        executor(mock),
        0,
        ClassifierConfig::default(),
    )
}

// Keep the socket bound: any attempted store read/write, audit, cap or admission
// query creates an observable connection even when a runtime hides SQL errors.
async fn tripwire() -> (PgPool, tokio::net::TcpListener) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("SQL tripwire");
    let options = sqlx::postgres::PgConnectOptions::new()
        .host("127.0.0.1")
        .port(listener.local_addr().expect("bound port").port())
        .username("preview_fixture")
        .database("preview_fixture")
        .ssl_mode(sqlx::postgres::PgSslMode::Disable);
    (
        PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(100))
            .connect_lazy_with(options),
        listener,
    )
}
fn no_sql(listener: tokio::net::TcpListener) {
    let listener = listener.into_std().expect("nonblocking listener");
    assert!(
        matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "preview/refusal attempted database access (including read/admission/audit)"
    );
}
fn safe_message(message: &Value) -> &str {
    assert_eq!(message["allowed_mentions"]["parse"], json!([]));
    for kind in ["users", "roles"] {
        assert!(
            message["allowed_mentions"][kind].is_null()
                || message["allowed_mentions"][kind] == json!([])
        );
    }
    assert_ne!(message["allowed_mentions"]["replied_user"], true);
    let text = message["content"].as_str().expect("visible response");
    assert!(
        !text.is_empty() && text.encode_utf16().count() <= 2000,
        "bounded visible content"
    );
    text
}
fn private_result(requests: &[RestRequest]) -> Value {
    assert!(
        requests
            .iter()
            .all(|r| (r.method == "POST" && r.path.ends_with("/callback"))
                || (r.method == "PATCH" && r.path == COMPLETION)),
        "unexpected public/component/member/job REST effect: {:?}",
        requests
            .iter()
            .map(|r| (&r.method, &r.path))
            .collect::<Vec<_>>()
    );
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|r| r.path.ends_with("/callback"))
        .collect();
    assert_eq!(callbacks.len(), 1, "one private acknowledgement");
    let ack = body(callbacks[0]);
    assert_eq!(
        ack["data"]["flags"].as_u64().unwrap_or(0) & 64,
        64,
        "ephemeral response"
    );
    let message = match ack["type"].as_u64() {
        Some(4) => {
            assert_eq!(requests.len(), 1);
            ack["data"].clone()
        }
        Some(5) => {
            assert_eq!(requests.len(), 2, "one deferred completion; no followups");
            body(&requests[1])
        }
        other => panic!("private response or defer required, got {other:?}"),
    };
    safe_message(&message);
    assert!(
        message["components"].is_null() || message["components"] == json!([]),
        "no enter/publish/confirm component in a preview or refusal"
    );
    message
}
fn preview(message: &Value) -> &str {
    let text = safe_message(message);
    assert!(
        text.contains("Preview — not created"),
        "missing private preview: {text}"
    );
    assert!(
        text.contains("/giveaway create")
            && (text.contains("without preview") || text.contains("preview:false")),
        "explicit create-without-preview next action: {text}"
    );
    let lower = text.to_lowercase();
    assert!(
        lower.contains("not reserved") || lower.contains("does not reserve"),
        "no capacity reservation promise: {text}"
    );
    assert!(
        lower.contains("eligibility") || lower.contains("eligible"),
        "eligibility is not established: {text}"
    );
    assert!(
        lower.contains("not guaranteed")
            || lower.contains("may fail")
            || lower.contains("can fail"),
        "later create is not guaranteed: {text}"
    );
    assert!(!text.contains("gw:") && !text.contains("Nonce:") && !text.contains("Seed:"));
    text
}
async fn offline(interaction: Interaction, enabled: bool) -> Value {
    let (pool, listener) = tripwire().await;
    let mock = rest().await;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        runtime(&pool, &mock, enabled).handle(&interaction),
    )
    .await;
    let requests = mock.requests();
    mock.shutdown().await;
    pool.close().await;
    assert!(result
        .expect("bounded dispatch")
        .expect("dispatcher result"));
    no_sql(listener);
    private_result(&requests)
}
fn proposed_options() -> Vec<Value> {
    let mut values = options();
    values.push(flag(true));
    values
}

#[test]
fn published_create_exposes_optional_boolean_preview_without_a_new_command() {
    let wire = serde_json::to_value(command_to_twilight(
        &two_bot_core::feature_commands::giveaway_command(),
    ))
    .expect("wire registry");
    assert_eq!(wire["default_member_permissions"], "32");
    let create = wire["options"]
        .as_array()
        .expect("subcommands")
        .iter()
        .find(|v| v["name"] == "create")
        .expect("existing create");
    let preview: Vec<_> = create["options"]
        .as_array()
        .expect("options")
        .iter()
        .filter(|v| v["name"] == "preview")
        .collect();
    assert_eq!(
        preview.len(),
        1,
        "existing create must publish one preview option"
    );
    assert_eq!(preview[0]["type"], 5);
    assert_ne!(preview[0]["required"], true);
    assert!(preview[0]["description"]
        .as_str()
        .expect("option help")
        .to_lowercase()
        .contains("privat"));
    let reference = include_str!("../../../docs/commands.md");
    let section = reference
        .split("## `/giveaway`\n")
        .nth(1)
        .expect("giveaway reference")
        .split("\n## ")
        .next()
        .expect("reference section");
    assert!(
        section.contains("preview") && section.to_lowercase().contains("boolean"),
        "generated command reference must expose the optional boolean"
    );
}

#[tokio::test]
async fn preview_dispatch_discloses_proposal_requirements_and_invocation_channel_without_sql() {
    let mut values = proposed_options();
    // A forward-compatible channel argument must not redirect publication.
    values.push(option("channel", 7, json!("1550102030405060799")));
    let reply = offline(invocation(9101, values), true).await;
    let text = preview(&reply);
    for required in ["Friday prize", ROLE, "7", "3", "2", CHANNEL] {
        assert!(
            text.contains(required),
            "missing proposed field {required}: {text}"
        );
    }
    assert!(
        !text.contains("1550102030405060799"),
        "invocation channel is authoritative"
    );
    assert!(
        text.contains("<t:") && (text.contains(":F>") || text.contains(":f>")),
        "show a localized absolute closing instant, not only relative time: {text}"
    );
}

#[tokio::test]
async fn preview_accepts_duration_winner_age_and_unicode_boundaries() {
    for (index, minutes, winners, age, prize) in [
        (0, 1, 1, 0, "a".to_owned()),
        (1, 43200, 100, 3650, "😀".repeat(200)),
        (
            2,
            60,
            2,
            7,
            "賞品 @everyone @here <@3333> <@&4444>".to_owned(),
        ),
    ] {
        let mut values = proposed_options();
        replace(&mut values, "duration", 4, json!(minutes));
        replace(&mut values, "winners", 4, json!(winners));
        replace(&mut values, "min-account-age-days", 4, json!(age));
        replace(&mut values, "min-server-age-days", 4, json!(age));
        replace(&mut values, "prize", 3, json!(prize));
        let reply = offline(invocation(9110 + index, values), true).await;
        let text = preview(&reply);
        if index == 1 {
            assert!(
                text.contains(&prize),
                "valid scalar-count boundary retained"
            );
        }
        if index == 2 {
            assert!(text.contains("賞品"), "Unicode prize retained");
        }
    }
}

#[tokio::test]
async fn preview_succeeds_even_when_the_database_pool_is_closed() {
    let (pool, listener) = tripwire().await;
    pool.close().await;
    let mock = rest().await;
    assert!(runtime(&pool, &mock, true)
        .handle(&invocation(9120, proposed_options()))
        .await
        .expect("dispatch"));
    let requests = mock.requests();
    mock.shutdown().await;
    no_sql(listener);
    preview(&private_result(&requests));
}

#[tokio::test]
async fn legacy_create_positive_control_reaches_the_observed_store_seam() {
    let (pool, listener) = tripwire().await;
    let mock = rest().await;
    assert!(runtime(&pool, &mock, true)
        .handle(&invocation(9150, options()))
        .await
        .expect("legacy create dispatch"));
    let requests = mock.requests();
    mock.shutdown().await;
    pool.close().await;
    let listener = listener.into_std().expect("SQL observer");
    assert!(
        listener.accept().is_ok(),
        "positive control must observe the real store attempt"
    );
    let reply = private_result(&requests);
    assert_eq!(
        safe_message(&reply),
        "Giveaway unavailable; try again later.",
        "bounded error, no SQL/provider detail"
    );
}

#[tokio::test]
async fn preview_missing_required_fields_refuses_without_effects() {
    for (index, missing) in ["prize", "duration", "winners"].into_iter().enumerate() {
        let mut values = proposed_options();
        values.retain(|v| v["name"] != missing);
        let reply = offline(invocation(9160 + index as u64, values), true).await;
        assert!(safe_message(&reply).to_lowercase().contains("invalid"));
        assert!(!safe_message(&reply).contains("Preview — not created"));
    }
}

#[tokio::test]
async fn preview_omitted_requirements_and_legacy_duration_alias_are_valid() {
    let values = vec![
        option("prize", 3, json!("No restrictions")),
        option("duration-minutes", 4, json!(1)),
        option("winners", 4, json!(1)),
        flag(true),
    ];
    let reply = offline(invocation(9170, values), true).await;
    let text = preview(&reply);
    assert!(text.contains("No restrictions"));
    assert!(!text.contains(ROLE), "omitted role cannot be invented");
}

#[tokio::test]
async fn invalid_preview_inputs_refuse_privately_without_effects() {
    for (index, name, kind, value) in [
        (0, "prize", 3, json!("   ")),
        (1, "prize", 3, json!("😀".repeat(201))),
        (2, "duration", 4, json!(0)),
        (3, "duration", 4, json!(43201)),
        (4, "duration", 4, json!(i64::MAX)),
        (5, "winners", 4, json!(0)),
        (6, "winners", 4, json!(101)),
        (7, "required-role", 3, json!(ROLE)),
        (8, "min-account-age-days", 4, json!(-1)),
        (9, "min-account-age-days", 4, json!(3651)),
        (10, "min-server-age-days", 4, json!(-1)),
        (11, "min-server-age-days", 4, json!(3651)),
        (12, "duration", 3, json!("60")),
        (13, "winners", 5, json!(true)),
        (14, "min-account-age-days", 3, json!("7")),
        (15, "min-server-age-days", 3, json!("3")),
    ] {
        let mut values = proposed_options();
        replace(&mut values, name, kind, value);
        let reply = offline(invocation(9200 + index, values), true).await;
        let text = safe_message(&reply);
        assert!(
            !text.contains("Preview — not created"),
            "invalid {name} accepted: {text}"
        );
        assert!(
            text.to_lowercase().contains("invalid"),
            "actionable refusal for {name}: {text}"
        );
    }
}

#[tokio::test]
async fn wrong_typed_duplicate_and_unknown_options_never_fall_through_to_create() {
    let mut cases = vec![
        vec![option("preview", 3, json!("true"))],
        vec![option("preview", 3, json!("false"))],
        vec![option("preview", 4, json!(1))],
        vec![flag(true), flag(true)],
        vec![flag(false), flag(true)],
        vec![flag(false), flag(false)],
        vec![flag(true), option("preveiw", 5, json!(true))],
        vec![flag(false), option("unknown", 3, json!("ignored?"))],
    ];
    // Strictness applies to the whole create input, not just the new flag.
    for duplicate in options() {
        cases.push(vec![flag(true), duplicate]);
    }
    for (index, additions) in cases.into_iter().enumerate() {
        let mut values = options();
        values.extend(additions);
        let reply = offline(invocation(9300 + index as u64, values), true).await;
        assert!(safe_message(&reply).to_lowercase().contains("invalid"));
        assert!(!safe_message(&reply).contains("Preview — not created"));
    }
}

#[tokio::test]
async fn preview_preserves_staff_guild_activation_channel_and_actor_refusals() {
    for case in 0..6 {
        let mut interaction = invocation(9400 + case, proposed_options());
        match case {
            0 => {
                interaction.member.as_mut().expect("member").permissions =
                    Some(twilight_model::guild::Permissions::empty())
            }
            1 => interaction.guild_id = Some(twilight_model::id::Id::new(1550102030405060799)),
            2 => interaction.guild_id = None,
            3 => interaction.channel = None,
            4 => {
                interaction.member.as_mut().expect("member").user = None;
                interaction.user = None;
            }
            _ => {}
        }
        if case == 1 || case == 2 {
            // The existing guild fence deliberately ignores foreign guilds/DMs.
            let (pool, listener) = tripwire().await;
            let mock = rest().await;
            let handled = runtime(&pool, &mock, true)
                .handle(&interaction)
                .await
                .expect("guild fence");
            let requests = mock.requests();
            mock.shutdown().await;
            pool.close().await;
            no_sql(listener);
            assert!(!handled, "foreign guild/DM remains unowned");
            assert!(requests.is_empty(), "no cross-guild response");
        } else {
            let reply = offline(interaction, case != 5).await;
            assert!(
                !safe_message(&reply).contains("Preview — not created"),
                "authority/context case {case} admitted"
            );
        }
    }
}

fn sources() -> (Clock, Nonce, Arc<AtomicUsize>, String) {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    // Runtime-generated synthetic replay input, not a hard-coded crypto key.
    let value = format!("{:032x}", std::process::id());
    let returned = value.clone();
    (
        Arc::new(|| OffsetDateTime::from_unix_timestamp(NOW).expect("fixed clock")),
        Arc::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            returned.clone()
        }),
        calls,
        value,
    )
}

#[tokio::test]
async fn injected_preview_has_exact_closing_time_and_never_mints_draw_randomness() {
    let (pool, listener) = tripwire().await;
    let mock = rest().await;
    let (clock, nonce, calls, secret) = sources();
    let rt = GiveawayRuntime::new(pool.clone(), executor(&mock)).with_create_sources(clock, nonce);
    // command() is the real post-authorization runtime seam; its normal caller
    // has already deferred privately. Dispatcher privacy is separately tested.
    rt.command(&invocation(9500, proposed_options())).await;
    let requests = mock.requests();
    mock.shutdown().await;
    pool.close().await;
    no_sql(listener);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "preview must precede nonce generation"
    );
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, COMPLETION);
    let reply = body(&requests[0]);
    let text = preview(&reply);
    assert!(
        text.contains("<t:1800003600:F>") || text.contains("<t:1800003600:f>"),
        "deadline must use the injected clock plus 60 minutes: {text}"
    );
    assert!(!text.contains(&secret), "unrevealed draw input leaked");
}

#[tokio::test]
async fn baseline_source_adapter_control_is_explicit_and_has_no_effect() {
    let (pool, listener) = tripwire().await;
    pool.close().await;
    let mock = rest().await;
    let (clock, nonce, calls, _) = sources();
    let rt = <GiveawayRuntime as BaselineCreateSources>::with_create_sources(
        GiveawayRuntime::new(pool, executor(&mock)),
        clock,
        nonce,
    );
    drop(rt);
    let requests = mock.requests();
    mock.shutdown().await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(requests.is_empty());
    no_sql(listener);
}

#[tokio::test]
async fn invalid_preview_and_decode_refusals_never_invoke_the_nonce_source() {
    let (pool, listener) = tripwire().await;
    let mock = rest().await;
    let (clock, nonce, calls, _) = sources();
    let rt = GiveawayRuntime::new(pool.clone(), executor(&mock)).with_create_sources(clock, nonce);
    for case in 0..4 {
        let mut values = proposed_options();
        match case {
            0 => replace(&mut values, "prize", 3, json!("   ")),
            1 => replace(&mut values, "min-account-age-days", 4, json!(-1)),
            2 => values.push(flag(false)),
            _ => values.push(option("unknown", 3, json!("refuse"))),
        }
        rt.command(&invocation(9520 + case, values)).await;
    }
    let requests = mock.requests();
    mock.shutdown().await;
    pool.close().await;
    no_sql(listener);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "invalid/decode refusal must precede randomness"
    );
    assert_eq!(requests.len(), 4);
    for request in requests {
        assert_eq!(request.path, COMPLETION);
        assert!(safe_message(&body(&request))
            .to_lowercase()
            .contains("invalid"));
    }
}

async fn database() -> Option<TestDatabase> {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => {
            eprintln!("SKIP database preview controls: TWO_TEST_DATABASE_URL absent; not evidence");
            return None;
        }
        Err(_) => panic!("non-Unicode test bootstrap"),
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("authorized disposable test database; no credential fallback"),
    )
}
fn posts(requests: &[RestRequest]) -> Vec<Value> {
    requests
        .iter()
        .filter(|r| r.method == "POST" && r.path == format!("/api/v10/channels/{CHANNEL}/messages"))
        .map(body)
        .collect()
}
fn presentation(text: &str) -> Vec<&str> {
    // Compare only shared proposed-message lines, excluding preview disclaimer
    // and persisted id footer. At least prize, time and requirements must match.
    text.lines()
        .filter(|line| {
            line.starts_with("Giveaway:")
                || line.starts_with("Ends ")
                || line.starts_with("Requirements:")
                || line.contains("entrants ·")
        })
        .collect()
}

#[tokio::test]
async fn database_legacy_missing_preview_create_and_replay_control() {
    let Some(db) = database().await else {
        return;
    };
    let mock = rest().await;
    let rt = GiveawayRuntime::new(db.pool().clone(), executor(&mock));
    let interaction = invocation(9590, options());
    rt.command(&interaction).await;
    rt.command(&interaction).await;
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM giveaways WHERE guild_id = $1 AND id = '9590'")
            .bind(GUILD)
            .fetch_one(db.pool())
            .await
            .expect("legacy row count");
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM automation_audit_log WHERE guild_id = $1 AND action = 'giveaway.create'")
        .bind(GUILD).fetch_one(db.pool()).await.expect("legacy audit count");
    let requests = mock.requests();
    mock.shutdown().await;
    db.close().await.expect("drop owned fixture");
    assert_eq!(
        (rows, audits, posts(&requests).len()),
        (1, 1, 1),
        "legacy create/replay unchanged"
    );
}

#[tokio::test]
async fn database_missing_and_false_preview_use_real_create_and_injected_sources() {
    let Some(db) = database().await else {
        return;
    };
    let mock = rest().await;
    let (clock, nonce, calls, secret) = sources();
    let rt =
        GiveawayRuntime::new(db.pool().clone(), executor(&mock)).with_create_sources(clock, nonce);
    for (index, preview_flag) in [None, Some(false)].into_iter().enumerate() {
        let id = 9600 + index as u64;
        let mut values = options();
        if let Some(flag_value) = preview_flag {
            values.push(flag(flag_value));
        }
        let interaction = invocation(id, values);
        rt.command(&interaction).await;
        rt.command(&interaction).await;
        let row = sqlx::query("SELECT channel_id, prize, ends_at, draw_nonce, requirements_json FROM giveaways WHERE guild_id = $1 AND id = $2")
            .bind(GUILD).bind(id.to_string()).fetch_optional(db.pool()).await.expect("read committed create");
        let row = row.expect("missing/false preview must create");
        assert_eq!(row.get::<String, _>("channel_id"), CHANNEL);
        assert_eq!(row.get::<String, _>("prize"), "Friday prize");
        assert_eq!(
            row.get::<OffsetDateTime, _>("ends_at").unix_timestamp(),
            NOW + 3600
        );
        assert_eq!(
            row.get::<String, _>("draw_nonce"),
            secret,
            "nonce spy must observe real create"
        );
        assert_eq!(
            row.get::<Value, _>("requirements_json"),
            json!({"roles": [ROLE], "min_account_age_days": 7, "min_guild_age_days": 3})
        );
    }
    let requests = mock.requests();
    mock.shutdown().await;
    assert_eq!(posts(&requests).len(), 2, "replay must not republish");
    assert!(
        calls.load(Ordering::SeqCst) >= 2,
        "real create uses the injected source"
    );
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM automation_audit_log WHERE guild_id = $1 AND action = 'giveaway.create'")
        .bind(GUILD).fetch_one(db.pool()).await.expect("create audits");
    assert_eq!(audits, 2, "replay cannot duplicate audit");
    db.close().await.expect("drop owned fixture");
}

#[tokio::test]
async fn database_preview_and_create_share_message_fields_without_reservation_or_audit() {
    let Some(db) = database().await else {
        return;
    };
    let mock = rest().await;
    let sql_calls = Arc::new(AtomicUsize::new(0));
    let observer = sql_calls.clone();
    let observed_pool = PgPoolOptions::new()
        .max_connections(1)
        .before_acquire(move |_, _| {
            let observer = observer.clone();
            Box::pin(async move {
                observer.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            })
        })
        .connect_with(db.pool().connect_options().as_ref().clone())
        .await
        .expect("owned database observer");
    // Prime the sole connection so the hook sees the very first runtime SQL.
    drop(
        observed_pool
            .acquire()
            .await
            .expect("prime acquisition observer"),
    );
    sql_calls.store(0, Ordering::SeqCst);
    let (clock, nonce, calls, _) = sources();
    let rt = GiveawayRuntime::new(observed_pool.clone(), executor(&mock))
        .with_create_sources(clock, nonce);
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM automation_audit_log")
        .fetch_one(db.pool())
        .await
        .expect("baseline audit");
    let first = invocation(9700, proposed_options());
    let second = invocation(9701, proposed_options());
    let ((), ()) = tokio::join!(rt.command(&first), rt.command(&second));
    assert_eq!(
        sql_calls.load(Ordering::SeqCst),
        0,
        "preview cannot read or write the store"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM giveaways")
        .fetch_one(db.pool())
        .await
        .expect("rows");
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM automation_audit_log")
        .fetch_one(db.pool())
        .await
        .expect("audit");
    assert_eq!(
        (rows, after),
        (0, before),
        "no reservation/audit/scheduled draw row"
    );
    let proposal_requests = mock.requests();
    assert_eq!(proposal_requests.len(), 2, "only two private completions");
    let proposal = body(&proposal_requests[0]);
    let proposed = preview(&proposal);
    assert!(
        presentation(proposed).len() >= 4,
        "shared prize/count/deadline/requirements fields must be visible"
    );
    // Same interaction id is not reserved by preview: removing the flag creates.
    rt.command(&invocation(9700, options())).await;
    let requests = mock.requests();
    let public = posts(&requests);
    assert_eq!(public.len(), 1);
    let created = safe_message(&public[0]);
    assert_eq!(
        presentation(proposed),
        presentation(created),
        "shared pure validated presentation must not drift"
    );
    assert!(
        public[0]["components"].to_string().contains("gw:9700"),
        "real create retains live entry"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    mock.shutdown().await;
    drop(rt);
    observed_pool.close().await;
    db.close().await.expect("drop owned fixture");
}
