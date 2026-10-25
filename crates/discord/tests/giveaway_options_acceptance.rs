//! Protected RED-first acceptance for strict giveaway management decoding.
//! Exercises the existing routed runtime, not a proposed decoder API. The
//! paired implementation must preserve this file. No real Discord traffic.
//!
//! Breaks caught: first/last-match duplicate selection, alias precedence,
//! ignored unknown/list fields, malformed optional defaults, pre-validation
//! store acquisition and lifecycle effects, unsafe refusal text, lost ACK or
//! authorization ownership, and regressions to valid aliases/entry buttons.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tracing::instrument::WithSubscriber;
use twilight_model::application::interaction::Interaction;
use two_bot_core::{commands::PERM_MANAGE_GUILD, ClassifierConfig, InteractionRouter, RouterGates};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1550102030405060701";
const CHANNEL: &str = "1550102030405060702";
const OTHER_CHANNEL: &str = "1550102030405060703";
const ACTOR: &str = "1550102030405060711";
const ROLE: &str = "1550102030405060731";
const OTHER_ROLE: &str = "1550102030405060732";
const TARGET: &str = "options_target";
const OTHER_TARGET: &str = "options_other";
const SENTINEL: &str =
    "synthetic-option-sentinel @everyone <@1550102030405060711> SELECT prize FROM private_fixture";
const SUBCOMMANDS: [&str; 8] = [
    "create", "list", "cancel", "end", "pause", "resume", "extend", "reroll",
];

fn gates(enabled: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD.parse().expect("fixture guild")),
        scorecard: true,
        automations: enabled,
        announcements: enabled,
        moderation: true,
        voice: true,
        voice_assistant: true,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    }
}

fn option(name: &str, kind: u8, value: Value) -> Value {
    json!({"name": name, "type": kind, "value": value})
}

fn create_options(alias: &str) -> Vec<Value> {
    vec![
        option("prize", 3, json!("Fixture prize")),
        option(alias, 4, json!(60)),
        option("winners", 4, json!(1)),
    ]
}

fn options(sub: &str) -> Vec<Value> {
    match sub {
        "create" => create_options("duration"),
        "list" => vec![],
        "extend" => vec![
            option("id", 3, json!(TARGET)),
            option("duration-minutes", 4, json!(60)),
        ],
        _ => vec![option("id", 3, json!(TARGET))],
    }
}

fn payload(sub: &str, children: Vec<Value>) -> Value {
    json!([{"name": sub, "type": 1, "options": children}])
}

fn slash(id: u64, permissions: u64, options: Value) -> Interaction {
    serde_json::from_value(json!({
        "application_id": "1111", "authorizing_integration_owners": {"0": GUILD},
        "id": id.to_string(), "token": "mock-options-token", "type": 2, "version": 1,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
        "member": {"permissions": permissions.to_string(), "roles": [],
            "deaf": false, "mute": false, "flags": 0,
            "user": {"id": ACTOR, "username": "fixture", "discriminator": "0"}},
        "data": {"id": "4444", "name": "giveaway", "type": 1, "options": options}
    }))
    .expect("decode fixture interaction with real Twilight types")
}

fn button(id: u64) -> Interaction {
    let mut wire = serde_json::to_value(slash(id, 0, json!([]))).expect("fixture serialization");
    wire["type"] = json!(3);
    wire["data"] = json!({"custom_id": format!("gw:{TARGET}"), "component_type": 2});
    serde_json::from_value(wire).expect("entry interaction")
}

struct Fixture {
    db: TestDatabase,
    observed: PgPool,
    acquisitions: Arc<AtomicUsize>,
    rest: MockRest,
    runtime: InteractionRuntime,
}

impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("required disposable test bootstrap; this suite never skips without a DB");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("guarded disposable test database");
        let acquisitions = Arc::new(AtomicUsize::new(0));
        let reused = Arc::clone(&acquisitions);
        let connected = Arc::clone(&acquisitions);
        // Both new and reused connections are observable. No source/store seam
        // is replaced: every real SQLx store access must acquire a connection.
        // No minimum means no background replenishment can increment the spy.
        let observed = PgPoolOptions::new()
            .min_connections(0)
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .before_acquire(move |_, _| {
                reused.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(true) })
            })
            .after_connect(move |_, _| {
                connected.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            })
            .connect_with(db.pool().connect_options().as_ref().clone())
            .await
            .expect("observed pool confined to the guarded fixture database");
        let rest = MockRest::with_responder(|request: &RestRequest| {
            if request.path.contains("/interactions/") {
                ScriptedResponse::status(204)
            } else if request.path.contains("/members/") {
                ScriptedResponse::json(200, json!({"user": {"id": ACTOR}, "roles": []}))
            } else {
                ScriptedResponse::json(200, json!({"id": "99"}))
            }
        })
        .await;
        let runtime = Self::runtime(&observed, &rest, true);
        Self {
            db,
            observed,
            acquisitions,
            rest,
            runtime,
        }
    }

    fn runtime(pool: &PgPool, rest: &MockRest, enabled: bool) -> InteractionRuntime {
        InteractionRuntime::with_router(
            InteractionRouter::new(gates(enabled)),
            pool.clone(),
            ActionExecutor::with_proxy("mock-not-a-credential".into(), Some(rest.origin()))
                .expect("loopback executor"),
            0,
            ClassifierConfig::default(),
        )
    }

    async fn seed(&self) {
        // Only this owned disposable fixture is reset; never a bootstrap or
        // application database. Two distinguishable rows expose winner-picking.
        sqlx::raw_sql("DELETE FROM giveaway_draws; DELETE FROM giveaway_entries; DELETE FROM giveaways; DELETE FROM automation_audit_log;")
            .execute(self.db.pool()).await.expect("reset owned fixture");
        for id in [TARGET, OTHER_TARGET] {
            sqlx::query("INSERT INTO giveaways (guild_id,id,prize,channel_id,message_id,created_by,winners_count,ends_at,status,requirements_json,max_entries,draw_nonce,created_at,updated_at) VALUES ($1,$2,'Fixture prize',$3,'99',$4,1,now()+interval '1 hour','open','{}',100,$5,now(),now())")
                .bind(GUILD).bind(id).bind(CHANNEL).bind(ACTOR)
                .bind(two_bot_core::giveaway::mint_draw_nonce())
                .execute(self.db.pool()).await.expect("seed giveaway");
        }
    }

    async fn snapshot(&self) -> Value {
        sqlx::query_scalar("SELECT jsonb_build_object('giveaways',(SELECT coalesce(jsonb_agg(to_jsonb(g) ORDER BY id),'[]'::jsonb) FROM giveaways g),'entries',(SELECT coalesce(jsonb_agg(to_jsonb(e) ORDER BY giveaway_id,user_id),'[]'::jsonb) FROM giveaway_entries e),'draws',(SELECT coalesce(jsonb_agg(to_jsonb(d) ORDER BY giveaway_id,draw_no),'[]'::jsonb) FROM giveaway_draws d),'audit',(SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY id),'[]'::jsonb) FROM automation_audit_log a))")
            .fetch_one(self.db.pool()).await.expect("snapshot persistent effects")
    }

    async fn close(self) {
        self.observed.close().await;
        self.rest.shutdown().await;
        self.db.close().await.expect("drop disposable fixture");
    }
}

// Allows either an immediate private callback or private defer + one edit.
// Does not pin decoder layout, exact copy, or a particular refusal enum.
fn reply_contract(requests: &[RestRequest]) -> Result<String, String> {
    let mut callbacks = 0;
    let mut private = false;
    let mut texts = Vec::new();
    for request in requests {
        let body: Value = serde_json::from_slice(&request.body).map_err(|_| "non-JSON reply")?;
        let data = if request.method == "POST" && request.path.contains("/interactions/") {
            callbacks += 1;
            let kind = body["type"].as_u64().ok_or("callback type missing")?;
            if kind != 4 && kind != 5 {
                return Err("unexpected ACK type".into());
            }
            private = body["data"]["flags"].as_u64().unwrap_or(0) & 64 != 0;
            if !private {
                return Err("public ACK".into());
            }
            &body["data"]
        } else if request.method == "PATCH"
            && request.path.contains("/webhooks/")
            && (request.path.ends_with("/@original") || request.path.ends_with("/%40original"))
        {
            if !private {
                return Err("edit without private ACK".into());
            }
            &body
        } else {
            return Err(format!(
                "non-reply REST effect: {} {}",
                request.method, request.path
            ));
        };
        if let Some(text) = data["content"].as_str().filter(|text| !text.is_empty()) {
            let mentions = &data["allowed_mentions"];
            if mentions["parse"]
                .as_array()
                .is_none_or(|values| !values.is_empty())
                || mentions["users"]
                    .as_array()
                    .is_some_and(|values| !values.is_empty())
                || mentions["roles"]
                    .as_array()
                    .is_some_and(|values| !values.is_empty())
                || mentions["replied_user"].as_bool() == Some(true)
            {
                return Err("reply permits mentions".into());
            }
            if text.encode_utf16().count() > 2000 {
                return Err("unbounded reply".into());
            }
            for forbidden in [
                "synthetic-option-sentinel",
                "SELECT prize",
                "private_fixture",
                "@everyone",
                ROLE,
                OTHER_ROLE,
            ] {
                if text.contains(forbidden) {
                    return Err("submitted/private value echoed".into());
                }
            }
            texts.push(text.to_owned());
        }
    }
    if callbacks != 1 || texts.len() != 1 {
        return Err(format!(
            "expected one ACK and one final reply; got {callbacks}/{}",
            texts.len()
        ));
    }
    Ok(texts.remove(0))
}

struct Case {
    label: String,
    payload: Value,
    repair_option: &'static str,
}

impl Case {
    fn new(sub: &str, label: &str, children: Vec<Value>, repair_option: &'static str) -> Self {
        Self {
            label: format!("{sub}/{label}"),
            payload: payload(sub, children),
            repair_option,
        }
    }
}

#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("captured log")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn malformed_cases(cases: Vec<Case>) {
    let fixture = Fixture::new().await;
    let mut failures = Vec::new();
    for (index, case) in cases.iter().enumerate() {
        fixture.seed().await;
        let before = fixture.snapshot().await;
        let rest_before = fixture.rest.requests().len();
        fixture.acquisitions.store(0, Ordering::SeqCst);
        let logs = CapturedLog::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        let handled = fixture
            .runtime
            .handle(&slash(
                20000 + index as u64,
                PERM_MANAGE_GUILD,
                case.payload.clone(),
            ))
            .with_subscriber(subscriber)
            .await;
        let acquisitions = fixture.acquisitions.load(Ordering::SeqCst);
        let after = fixture.snapshot().await;
        let requests = fixture.rest.requests();
        let reply = reply_contract(&requests[rest_before..]);
        let mut reasons = Vec::new();
        let captured = logs.0.lock().expect("captured log");
        let captured = String::from_utf8_lossy(&captured);
        for forbidden in [
            "synthetic-option-sentinel",
            "SELECT prize",
            "private_fixture",
            ROLE,
            OTHER_ROLE,
        ] {
            if captured.contains(forbidden) {
                reasons.push("untrusted input leaked into logs".into());
            }
        }
        if !matches!(handled, Ok(true)) {
            reasons.push("dispatch did not own interaction".to_owned());
        }
        if acquisitions != 0 {
            reasons.push(format!(
                "store acquired {acquisitions} connection(s) before refusal"
            ));
        }
        if after != before {
            reasons.push("persistent giveaway/entry/draw/audit state changed".to_owned());
        }
        match reply {
            Err(reason) => reasons.push(reason),
            Ok(text) => {
                let lower = text.to_lowercase();
                if !lower.contains(case.repair_option) {
                    reasons.push(format!(
                        "refusal does not name supported repair option {}",
                        case.repair_option
                    ));
                }
                if ![
                    "remove", "provide", "use ", "choose", "supply", "specify", "include",
                    "select", "enter", "re-run", "rerun", "pick ",
                ]
                .iter()
                .any(|verb| lower.contains(verb))
                {
                    reasons.push("refusal lacks concrete repair action".into());
                }
                if lower.contains("try again later") {
                    reasons.push("decode error became a store-outage reply".into());
                }
            }
        }
        if !reasons.is_empty() {
            failures.push(format!("{}: {}", case.label, reasons.join("; ")));
        }
    }
    let count = cases.len();
    fixture.close().await;
    assert!(
        failures.is_empty(),
        "{} of {count} malformed cases violated strict decoding:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[tokio::test]
async fn every_management_branch_rejects_unknown_options_before_store_or_lifecycle() {
    malformed_cases(
        SUBCOMMANDS
            .iter()
            .map(|sub| {
                let mut children = options(sub);
                children.push(option(SENTINEL, 3, json!(SENTINEL)));
                Case::new(sub, "unknown-field", children, "giveaway")
            })
            .collect(),
    )
    .await;
}

#[tokio::test]
async fn lifecycle_ids_are_required_unique_typed_and_bounded_before_store() {
    let mut cases = Vec::new();
    for sub in ["cancel", "end", "pause", "resume", "extend", "reroll"] {
        let rest: Vec<_> = options(sub)
            .into_iter()
            .filter(|option| option["name"] != "id")
            .collect();
        cases.push(Case::new(sub, "missing-id", rest.clone(), "id"));
        for bad in [
            option("id", 4, json!(42)),
            option("id", 3, json!("")),
            option("id", 3, json!("x".repeat(65))),
            option("id", 3, json!("invalid/id")),
            option("id", 3, json!(SENTINEL)),
        ] {
            let mut children = rest.clone();
            children.push(bad);
            cases.push(Case::new(sub, "invalid-id", children, "id"));
        }
        for duplicate in [
            option("id", 3, json!(OTHER_TARGET)),
            option("id", 3, json!(TARGET)),
            option("id", 4, json!(7)),
        ] {
            for reverse in [false, true] {
                let mut children = options(sub);
                children.push(duplicate.clone());
                if reverse {
                    children.reverse();
                }
                cases.push(Case::new(sub, "duplicate-id", children, "id"));
            }
        }
    }
    malformed_cases(cases).await;
}

#[tokio::test]
async fn create_rejects_duplicate_fields_and_never_defaults_malformed_requirements() {
    let mut cases = Vec::new();
    for (name, good, bad) in [
        (
            "prize",
            option("prize", 3, json!(SENTINEL)),
            option("prize", 4, json!(1)),
        ),
        (
            "winners",
            option("winners", 4, json!(2)),
            option("winners", 3, json!(SENTINEL)),
        ),
        (
            "required-role",
            option("required-role", 8, json!(ROLE)),
            option("required-role", 3, json!(SENTINEL)),
        ),
        (
            "min-account-age-days",
            option("min-account-age-days", 4, json!(30)),
            option("min-account-age-days", 3, json!(SENTINEL)),
        ),
        (
            "min-server-age-days",
            option("min-server-age-days", 4, json!(7)),
            option("min-server-age-days", 3, json!(SENTINEL)),
        ),
        (
            "channel",
            option("channel", 7, json!(CHANNEL)),
            option("channel", 3, json!(SENTINEL)),
        ),
    ] {
        let mut base = create_options("duration");
        if !base.iter().any(|value| value["name"] == name) {
            base.push(good.clone());
        }
        for extra in [good, bad] {
            for reverse in [false, true] {
                let mut children = base.clone();
                children.push(extra.clone());
                if reverse {
                    children.reverse();
                }
                cases.push(Case::new("create", "duplicate-field", children, name));
            }
        }
        let mut wrong_only = create_options("duration");
        wrong_only.retain(|value| value["name"] != name);
        let wrong = option(name, 3, json!(SENTINEL));
        // Prize is already a string; use a genuinely wrong type for that field.
        wrong_only.push(if name == "prize" {
            option(name, 4, json!(1))
        } else {
            wrong
        });
        cases.push(Case::new("create", "wrong-type", wrong_only, name));
    }
    let mut roles = create_options("duration");
    roles.extend([
        option("required-role", 8, json!(ROLE)),
        option("required-role", 8, json!(OTHER_ROLE)),
    ]);
    cases.push(Case::new(
        "create",
        "contradictory-roles",
        roles,
        "required-role",
    ));
    for name in ["min-account-age-days", "min-server-age-days"] {
        let mut ages = create_options("duration");
        ages.extend([option(name, 4, json!(0)), option(name, 4, json!(3650))]);
        cases.push(Case::new("create", "contradictory-ages", ages, name));
        for age in [-1, 3651] {
            let mut children = create_options("duration");
            children.push(option(name, 4, json!(age)));
            cases.push(Case::new("create", "age-out-of-range", children, name));
        }
    }
    for name in ["prize", "duration", "winners"] {
        let mut children = create_options("duration");
        children.retain(|value| value["name"] != name);
        cases.push(Case::new("create", "missing-required", children, name));
    }
    for (name, value) in [
        ("prize", json!("")),
        ("prize", json!("x".repeat(201))),
        ("winners", json!(0)),
        ("winners", json!(101)),
    ] {
        let mut children = create_options("duration");
        children
            .iter_mut()
            .find(|value| value["name"] == name)
            .expect("field")["value"] = value;
        cases.push(Case::new("create", "out-of-range", children, name));
    }
    malformed_cases(cases).await;
}

#[tokio::test]
async fn duration_aliases_cannot_compete_or_hide_wrong_typed_duplicates() {
    let mut cases = Vec::new();
    for sub in ["create", "extend"] {
        for alias in ["duration", "duration-minutes"] {
            let mut base = if sub == "create" {
                create_options(alias)
            } else {
                vec![option("id", 3, json!(TARGET)), option(alias, 4, json!(60))]
            };
            let other = if alias == "duration" {
                "duration-minutes"
            } else {
                "duration"
            };
            for extra in [
                option(other, 4, json!(60)),
                option(other, 4, json!(120)),
                option(alias, 4, json!(60)),
                option(alias, 3, json!(SENTINEL)),
                option(other, 3, json!(SENTINEL)),
            ] {
                for reverse in [false, true] {
                    let mut children = base.clone();
                    children.push(extra.clone());
                    if reverse {
                        children.reverse();
                    }
                    cases.push(Case::new(sub, "competing-duration", children, "duration"));
                }
            }
            let mut absent = base.clone();
            absent.retain(|value| value["name"] != alias);
            cases.push(Case::new(
                sub,
                "missing-duration",
                absent.clone(),
                "duration",
            ));
            absent.push(option(alias, 3, json!(SENTINEL)));
            cases.push(Case::new(sub, "wrong-duration-type", absent, "duration"));
            for minutes in [0, -1, 43201, i64::MAX] {
                base.iter_mut()
                    .find(|value| value["name"] == alias)
                    .expect("duration")["value"] = json!(minutes);
                cases.push(Case::new(
                    sub,
                    "duration-out-of-range",
                    base.clone(),
                    "duration",
                ));
            }
        }
    }
    malformed_cases(cases).await;
}

#[tokio::test]
async fn reroll_target_is_optional_but_must_be_a_single_user() {
    let mut cases = Vec::new();
    for extra in [
        option("user", 6, json!(ACTOR)),
        option("user", 6, json!(OTHER_ROLE)),
        option("user", 3, json!(SENTINEL)),
        option("user", 8, json!(ROLE)),
    ] {
        for reverse in [false, true] {
            let mut children = options("reroll");
            children.extend([option("user", 6, json!(ACTOR)), extra.clone()]);
            if reverse {
                children.reverse();
            }
            cases.push(Case::new("reroll", "duplicate-target", children, "user"));
        }
    }
    for bad in [
        option("user", 3, json!(SENTINEL)),
        option("user", 8, json!(ROLE)),
    ] {
        let mut children = options("reroll");
        children.push(bad);
        cases.push(Case::new("reroll", "wrong-target-type", children, "user"));
    }
    malformed_cases(cases).await;
}

#[tokio::test]
async fn malformed_subcommand_shapes_do_not_reach_store() {
    let mut cases = vec![
        Case {
            label: "missing-subcommand".into(),
            payload: json!([]),
            repair_option: "giveaway",
        },
        Case {
            label: "two-subcommands".into(),
            payload: json!([{"name":"list","type":1,"options":[]},{"name":"cancel","type":1,"options":[option("id",3,json!(TARGET))]}]),
            repair_option: "giveaway",
        },
        Case {
            label: "wrong-subcommand-type".into(),
            payload: json!([option("list", 3, json!(SENTINEL))]),
            repair_option: "giveaway",
        },
        Case {
            label: "nested-subcommand-group".into(),
            payload: json!([{"name":"list","type":2,"options":[{"name":"cancel","type":1,"options":[option("id",3,json!(TARGET))]}]}]),
            repair_option: "giveaway",
        },
        Case::new("not-a-command", "unsupported", vec![], "giveaway"),
    ];
    for sub in SUBCOMMANDS {
        let mut children = options(sub);
        children.push(json!({"name":"list","type":1,"options":[]}));
        cases.push(Case::new(sub, "nested-subcommand", children, "giveaway"));
    }
    malformed_cases(cases).await;
}

#[tokio::test]
async fn valid_single_duration_aliases_preserve_create_requirements_and_invocation_channel() {
    let fixture = Fixture::new().await;
    for (index, alias) in ["duration", "duration-minutes"].iter().enumerate() {
        fixture.seed().await;
        let mut children = create_options(alias);
        children.extend([
            option("required-role", 8, json!(ROLE)),
            option("min-account-age-days", 4, json!(30)),
            option("min-server-age-days", 4, json!(7)),
            option("channel", 7, json!(OTHER_CHANNEL)),
        ]);
        let id = 30000 + index as u64;
        fixture.acquisitions.store(0, Ordering::SeqCst);
        assert!(fixture
            .runtime
            .handle(&slash(id, PERM_MANAGE_GUILD, payload("create", children)))
            .await
            .expect("create dispatch"));
        assert!(
            fixture.acquisitions.load(Ordering::SeqCst) > 0,
            "spy must observe a valid store operation"
        );
        let row: (String, i32, Value, i64) = sqlx::query_as("SELECT channel_id,winners_count,requirements_json,extract(epoch FROM ends_at-created_at)::bigint FROM giveaways WHERE id=$1")
            .bind(id.to_string()).fetch_one(fixture.db.pool()).await.expect("valid alias creates a row");
        assert_eq!(row.0, CHANNEL, "compat channel cannot steer the post");
        assert_eq!(row.1, 1);
        assert_eq!(
            row.2,
            json!({"roles":[ROLE],"min_account_age_days":30,"min_guild_age_days":7})
        );
        assert_eq!(row.3, 3600);
        assert!(fixture
            .rest
            .requests()
            .iter()
            .any(|request| request.method == "POST"
                && request.path == format!("/api/v10/channels/{CHANNEL}/messages")));
    }
    fixture.close().await;
}

#[tokio::test]
async fn valid_lifecycle_commands_and_each_extend_alias_still_execute() {
    let fixture = Fixture::new().await;
    for (index, sub) in [
        "list", "cancel", "end", "pause", "resume", "extend", "reroll",
    ]
    .iter()
    .enumerate()
    {
        fixture.seed().await;
        if *sub == "resume" {
            sqlx::query("UPDATE giveaways SET status='paused',paused_at=now()-interval '10 seconds' WHERE id=$1")
                .bind(TARGET).execute(fixture.db.pool()).await.expect("paused control");
        }
        if *sub == "reroll" {
            assert!(fixture
                .runtime
                .handle(&slash(
                    31000,
                    PERM_MANAGE_GUILD,
                    payload("end", options("end"))
                ))
                .await
                .expect("draw setup"));
        }
        let before = fixture.rest.requests().len();
        fixture.acquisitions.store(0, Ordering::SeqCst);
        assert!(fixture
            .runtime
            .handle(&slash(
                32000 + index as u64,
                PERM_MANAGE_GUILD,
                payload(sub, options(sub))
            ))
            .await
            .expect("valid dispatch"));
        assert!(
            fixture.acquisitions.load(Ordering::SeqCst) > 0,
            "valid {sub} must reach the real store"
        );
        let snapshot = fixture.snapshot().await;
        let row = snapshot["giveaways"]
            .as_array()
            .expect("rows")
            .iter()
            .find(|row| row["id"] == TARGET)
            .expect("target");
        match *sub {
            "list" => {
                let requests = fixture.rest.requests();
                assert!(reply_contract(&requests[before..])
                    .expect("private list")
                    .contains(TARGET));
            }
            "cancel" => assert_eq!(row["status"], "cancelled"),
            "pause" => assert_eq!(row["status"], "paused"),
            "resume" => assert_eq!(row["status"], "open"),
            "end" | "reroll" => {
                assert_eq!(row["status"], "drawn");
                assert_eq!(
                    snapshot["draws"].as_array().expect("draws").len(),
                    if *sub == "reroll" { 2 } else { 1 }
                );
            }
            "extend" => {
                let seconds: i64 = sqlx::query_scalar("SELECT extract(epoch FROM ends_at-created_at)::bigint FROM giveaways WHERE id=$1")
                    .bind(TARGET).fetch_one(fixture.db.pool()).await.expect("extended deadline");
                assert_eq!(seconds, 7200);
            }
            _ => unreachable!(),
        }
    }
    for alias in ["duration", "duration-minutes"] {
        fixture.seed().await;
        let children = vec![option("id", 3, json!(TARGET)), option(alias, 4, json!(1))];
        assert!(fixture
            .runtime
            .handle(&slash(
                33000,
                PERM_MANAGE_GUILD,
                payload("extend", children)
            ))
            .await
            .expect("extend alias"));
        let seconds: i64 = sqlx::query_scalar(
            "SELECT extract(epoch FROM ends_at-created_at)::bigint FROM giveaways WHERE id=$1",
        )
        .bind(TARGET)
        .fetch_one(fixture.db.pool())
        .await
        .expect("deadline");
        assert_eq!(seconds, 3660, "single {alias} remains compatible");
    }
    fixture.close().await;
}

#[tokio::test]
async fn permission_gate_foreign_guild_and_failed_ack_never_execute_management() {
    let fixture = Fixture::new().await;
    fixture.seed().await;
    let snapshot = fixture.snapshot().await;
    for (index, sub) in SUBCOMMANDS.iter().enumerate() {
        let before = fixture.rest.requests().len();
        fixture.acquisitions.store(0, Ordering::SeqCst);
        assert!(fixture
            .runtime
            .handle(&slash(34000 + index as u64, 0, payload(sub, options(sub))))
            .await
            .expect("denial owned"));
        assert_eq!(fixture.acquisitions.load(Ordering::SeqCst), 0);
        let requests = fixture.rest.requests();
        let refusal = reply_contract(&requests[before..]).expect("one private permission refusal");
        assert!(refusal.to_lowercase().contains("permission"));
    }
    let disabled = Fixture::runtime(&fixture.observed, &fixture.rest, false);
    fixture.acquisitions.store(0, Ordering::SeqCst);
    assert!(disabled
        .handle(&slash(
            35000,
            PERM_MANAGE_GUILD,
            payload("create", options("create"))
        ))
        .await
        .expect("disabled gate"));
    assert_eq!(fixture.acquisitions.load(Ordering::SeqCst), 0);
    let mut foreign = slash(
        35001,
        PERM_MANAGE_GUILD,
        payload("cancel", options("cancel")),
    );
    foreign.guild_id = Some(9999.into());
    let before = fixture.rest.requests().len();
    assert!(!fixture
        .runtime
        .handle(&foreign)
        .await
        .expect("foreign ignored"));
    assert_eq!(fixture.rest.requests().len(), before);
    assert_eq!(fixture.snapshot().await, snapshot);
    let failed = MockRest::start(vec![], ScriptedResponse::status(400)).await;
    let runtime = Fixture::runtime(&fixture.observed, &failed, true);
    fixture.acquisitions.store(0, Ordering::SeqCst);
    assert!(runtime
        .handle(&slash(
            35002,
            PERM_MANAGE_GUILD,
            payload("cancel", options("cancel"))
        ))
        .await
        .expect("failed ACK owned"));
    assert_eq!(fixture.acquisitions.load(Ordering::SeqCst), 0);
    assert_eq!(failed.requests().len(), 1);
    assert_eq!(fixture.snapshot().await, snapshot);
    failed.shutdown().await;
    fixture.close().await;
}

#[tokio::test]
async fn malformed_management_does_not_consume_member_entry_budget_or_steal_entry_ack() {
    let fixture = Fixture::new().await;
    fixture.seed().await;
    // Same actor, same runtime. More than the existing ten-write entry budget.
    for index in 0..12 {
        let mut children = options("cancel");
        children.push(option("id", 3, json!(OTHER_TARGET)));
        fixture
            .runtime
            .handle(&slash(
                36000 + index,
                PERM_MANAGE_GUILD,
                payload("cancel", children),
            ))
            .await
            .expect("malformed management owned");
    }
    // Restore fixture state if the RED baseline cancelled either target; the
    // runtime/budget object is deliberately NOT replaced or reset.
    fixture.seed().await;
    let before = fixture.rest.requests().len();
    assert!(fixture
        .runtime
        .handle(&button(37000))
        .await
        .expect("member entry owned"));
    let entries: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM giveaway_entries WHERE giveaway_id=$1 AND user_id=$2",
    )
    .bind(TARGET)
    .bind(ACTOR)
    .fetch_one(fixture.db.pool())
    .await
    .expect("entry row");
    assert_eq!(
        entries, 1,
        "strict management validation must leave normal member entry working"
    );
    let requests = fixture.rest.requests();
    // Entry legitimately edits the public giveaway count; only inspect its
    // interaction reply and do not outlaw that normal component effect.
    let replies: Vec<_> = requests[before..]
        .iter()
        .filter(|request| {
            request.path.contains("/interactions/") || request.path.contains("/webhooks/")
        })
        .cloned()
        .collect();
    reply_contract(&replies).expect("one private entry completion, no management fallthrough");
    fixture.close().await;
}

#[tokio::test]
async fn valid_boundary_values_and_absent_requirements_remain_accepted() {
    let fixture = Fixture::new().await;
    for (index, (minutes, winners, prize_len, age)) in [
        (1, 1, 1, None),
        (43200, 100, 200, Some(3650)),
        (60, 1, 10, Some(0)),
    ]
    .into_iter()
    .enumerate()
    {
        fixture.seed().await;
        let mut children = vec![
            option("prize", 3, json!("x".repeat(prize_len))),
            option("duration", 4, json!(minutes)),
            option("winners", 4, json!(winners)),
        ];
        if let Some(age) = age {
            children.extend([
                option("min-account-age-days", 4, json!(age)),
                option("min-server-age-days", 4, json!(age)),
            ]);
        }
        let id = 38000 + index as u64;
        assert!(fixture
            .runtime
            .handle(&slash(id, PERM_MANAGE_GUILD, payload("create", children)))
            .await
            .expect("valid boundary"));
        let row: (String, i32, Value, i64) = sqlx::query_as("SELECT prize,winners_count,requirements_json,extract(epoch FROM ends_at-created_at)::bigint FROM giveaways WHERE id=$1")
            .bind(id.to_string()).fetch_one(fixture.db.pool()).await.expect("boundary row");
        assert_eq!(row.0.len(), prize_len);
        assert_eq!(row.1, winners);
        assert_eq!(
            row.2,
            json!({"roles":[],"min_account_age_days":age.unwrap_or(0),"min_guild_age_days":age.unwrap_or(0)})
        );
        assert_eq!(row.3, minutes * 60);
    }
    fixture.close().await;
}

#[tokio::test]
async fn a_single_typed_reroll_user_is_not_rejected_as_ambiguous() {
    let fixture = Fixture::new().await;
    fixture.seed().await;
    for user in [ACTOR, OTHER_ROLE] {
        sqlx::query("INSERT INTO giveaway_entries (guild_id,giveaway_id,user_id,weight,entered_at) VALUES ($1,$2,$3,1,now())")
            .bind(GUILD).bind(TARGET).bind(user).execute(fixture.db.pool()).await.expect("eligible fixture member");
    }
    assert!(fixture
        .runtime
        .handle(&slash(
            39000,
            PERM_MANAGE_GUILD,
            payload("end", options("end"))
        ))
        .await
        .expect("initial draw"));
    let winners: Value = sqlx::query_scalar(
        "SELECT winner_ids FROM giveaway_draws WHERE giveaway_id=$1 AND draw_no=0",
    )
    .bind(TARGET)
    .fetch_one(fixture.db.pool())
    .await
    .expect("draw winners");
    let winner = winners[0].as_str().expect("one winner");
    let mut children = options("reroll");
    children.push(option("user", 6, json!(winner)));
    assert!(fixture
        .runtime
        .handle(&slash(
            39001,
            PERM_MANAGE_GUILD,
            payload("reroll", children)
        ))
        .await
        .expect("targeted reroll"));
    let (kind, replacements): (String, Value) = sqlx::query_as(
        "SELECT reroll_kind,winner_ids FROM giveaway_draws WHERE giveaway_id=$1 AND draw_no=1",
    )
    .bind(TARGET)
    .fetch_one(fixture.db.pool())
    .await
    .expect("new immutable round");
    assert_eq!(kind, format!("single:{winner}"));
    assert_eq!(replacements.as_array().expect("winners").len(), 1);
    assert_ne!(replacements[0], json!(winner));
    fixture.close().await;
}
