#![cfg(test)]
//! Review hardening for the private admin welcome preview.
//!
//! The frozen acceptance oracle pins the specified behavior; this module pins
//! the review findings without touching the frozen files: admission-Blocked
//! retry of the component edit, pre-admission yield of preview picker clicks,
//! registered-identity resolution at the onboarding boundary, and the
//! differentiating fixtures (a role-bearing invoker, refusal before settings,
//! unconfirmed-ACK guidance). It mirrors the frozen harness with its own
//! fixtures and schemas — never import from the frozen module.
//!
//! Disposable-DB only (plus offline tests): no live Discord, no
//! staging/production database, never `DATABASE_URL`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use futures_util::FutureExt as _;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use twilight_model::application::interaction::{Interaction, InteractionData};
use twilight_model::gateway::event::Event;
use twilight_model::gateway::payload::incoming::InteractionCreate;
use twilight_model::id::Id;
use two_bot_core::onboarding::*;
use two_bot_core::send_admission::{
    AdmissionError, AdmissionFuture, AdmissionPermit, SendAdmission, SendCompletion, SendCooldown,
    TokenKey,
};
use two_bot_discord::onboarding_messages::picker_components;
use two_bot_discord::ActionExecutor;

use crate::gateway::{build_pipeline, ensure_crypto_provider};
use crate::onboarding::{OnboardingJob, OnboardingRuntime};

const TEST_DATABASE: &str = "postgres://agent_test@agent-testdb:5432/postgres";
const NOW: i64 = 1_790_780_400_000;
/// First line of a rendered preview message: the shared busy lane keys its
/// yield on this banner.
const PREVIEW_BANNER_CONTENT: &str =
    "Preview — not a delivery · legacy · fixture 765432109876543210";

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

fn schema_name_at(nanos: u128) -> String {
    format!(
        "tog38635_preview_review_{}_{}_{}",
        std::process::id(),
        NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed),
        nanos
    )
}

struct TestSchema {
    pool: PgPool,
    schema: String,
}

impl TestSchema {
    async fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let schema = schema_name_at(nanos);
        let mut admin = PgConnection::connect(TEST_DATABASE)
            .await
            .expect("only agent-testdb, agent_test, empty password");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&mut admin)
            .await
            .unwrap();
        let pool = Self::pool(&schema).await;
        for migration in [
            include_str!("../../cutover/migrations/0001_funnel.sql"),
            include_str!("../../cutover/migrations/0190_onboarding.sql"),
            include_str!("../../cutover/migrations/0330_guild_settings.sql"),
            include_str!("../../cutover/migrations/0331_guild_settings_versions.sql"),
            include_str!("../../cutover/migrations/0332_guild_settings_allocator.sql"),
            include_str!("../../cutover/migrations/0333_guild_settings_revision.sql"),
            include_str!("../../cutover/migrations/0334_guild_settings_cas.sql"),
            // The onboarding writers consult the opt-out suppression table.
            include_str!("../../cutover/migrations/0433_privacy_opt_out.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&pool).await.unwrap();
        }
        Self { pool, schema }
    }

    async fn pool(schema: &str) -> PgPool {
        assert!(schema
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'));
        let schema = schema.to_owned();
        PgPoolOptions::new()
            .max_connections(4)
            .after_connect(move |connection, _| {
                let schema = schema.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(schema)
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(TEST_DATABASE)
            .await
            .expect("only agent-testdb")
    }

    async fn close(self) {
        self.pool.close().await;
        let mut admin = PgConnection::connect(TEST_DATABASE).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&mut admin)
        .await
        .unwrap();
        let remaining: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_namespace WHERE nspname = $1")
                .bind(&self.schema)
                .fetch_one(&mut admin)
                .await
                .unwrap();
        assert_eq!(remaining, 0, "only this test's schema removed");
    }
}

#[derive(Default)]
struct PreviewDiscordState {
    reject_callback: bool,
}

/// Strict responder: the defer callback and the completing edit only. Every
/// other route panics, proving the preview performs no member resolution,
/// channel post, role write or invite/voice/audit traffic.
async fn preview_discord(state: Arc<Mutex<PreviewDiscordState>>) -> MockRest {
    MockRest::with_responder(move |request: &RestRequest| {
        let state = state.lock().unwrap();
        let path = request.path.strip_prefix("/api/v10").unwrap();
        match (request.method.as_str(), path) {
            ("POST", path) if path.ends_with("/callback") => {
                if state.reject_callback {
                    return ScriptedResponse::status(403);
                }
                ScriptedResponse::status(204)
            }
            ("PATCH", path) if path.ends_with("/messages/@original") => {
                ScriptedResponse::json(200, json!({"id":"98"}))
            }
            _ => panic!(
                "preview performs no Discord read or write besides defer+edit: {} {}",
                request.method, path
            ),
        }
    })
    .await
}

fn admin_member(user_id: &str, permissions: Option<&str>, roles: &[&str]) -> Value {
    let mut member = json!({
        "roles": roles, "deaf": false, "mute": false, "flags": 0,
        "user": {"id": user_id, "username": "admin", "discriminator": "0", "bot": false},
    });
    if let Some(bits) = permissions {
        member["permissions"] = Value::String(bits.to_owned());
    }
    member
}

/// A `/welcome-preview` slash from admin member 44 in guild 22: the exact
/// Manage Server bit, zero options.
fn preview_slash(member: Value) -> Interaction {
    serde_json::from_value(json!({
        "application_id": "111", "authorizing_integration_owners": {"0": "22"},
        "id": "333", "token": "mock-callback", "type": 2, "version": 1, "guild_id": "22",
        "member": member,
        "data": {"id": "444", "name": "welcome-preview", "type": 1, "options": []},
    }))
    .expect("valid welcome-preview slash fixture")
}

fn preview() -> Interaction {
    preview_slash(admin_member("44", Some("32"), &[]))
}

/// A games-picker click on a bannered preview message. The yield fence keys
/// on the banner, so no render is needed to exercise shared admission.
fn preview_click(token: &str, id: &str, content: &str) -> Interaction {
    serde_json::from_value(json!({
        "application_id": "1111", "authorizing_integration_owners": {"0": "22"},
        "id": id, "token": token, "type": 3, "version": 1,
        "guild_id": "22", "channel_id": "12",
        "member": admin_member("44", Some("32"), &[]),
        "message": {
            "id": "98", "channel_id": "12", "type": 0,
            "author": {"id": "999", "username": "bot", "discriminator": "0", "bot": true},
            "content": content, "components": [], "flags": 64,
            "timestamp": "2026-10-01T00:00:00.000000+00:00",
            "attachments": [], "embeds": [], "mentions": [], "mention_roles": [],
            "mention_everyone": false, "pinned": false, "tts": false
        },
        "data": {"custom_id": GAME_SELECT_ID, "component_type": 3, "values": ["fallguys"]}
    }))
    .expect("preview picker click fixture")
}

/// A slash carrying an explicit registration identity, for hydrated-ID tests.
fn slash_with_identity(id: u64, name: &str) -> Interaction {
    let mut value = serde_json::to_value(preview()).unwrap();
    value["application_id"] = json!("1111");
    value["data"]["id"] = json!(id.to_string());
    value["data"]["name"] = json!(name);
    value["data"]["guild_id"] = json!("22");
    serde_json::from_value(value).expect("identity slash fixture")
}

fn job(interaction: Interaction) -> OnboardingJob {
    OnboardingJob::Interaction(Box::new(interaction))
}

fn hydrate_preview_identities(executor: &ActionExecutor) {
    let mut commands = two_bot_discord::publish_commands(&[
        two_bot_core::CommandDefinition::new("welcome-preview", "Synthetic preview command."),
        two_bot_core::CommandDefinition::new("help", "Synthetic help command."),
    ]);
    commands[0].id = Some(Id::new(9001));
    commands[1].id = Some(Id::new(9002));
    executor
        .command_identities()
        .replace_guild(1111, 22, &commands)
        .expect("hydrated preview/help registration");
}

fn vars(mode: &str, dry_run: bool) -> HashMap<String, String> {
    HashMap::from([
        ("DISCORD_GUILD_ID".into(), "22".into()),
        ("TWO_ONBOARDING_MODE".into(), mode.into()),
        (
            "TWO_ONBOARDING_DRY_RUN".into(),
            if dry_run { "1" } else { "0" }.into(),
        ),
        ("DISCORD_LANDING_CHANNEL_IDS".into(), "12".into()),
        ("DISCORD_GOODBYE_CHANNEL_IDS".into(), "13".into()),
        ("DISCORD_ANCHOR_WELCOME_CHANNEL_ID".into(), "14".into()),
        (
            "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID".into(),
            "10".into(),
        ),
        ("DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID".into(), "11".into()),
    ])
}

fn runtime(pool: &PgPool, mock: &MockRest, vars: &HashMap<String, String>) -> OnboardingRuntime {
    ensure_crypto_provider();
    let executor =
        ActionExecutor::with_proxy("onboarding-test-only-token".into(), Some(mock.origin()))
            .unwrap();
    OnboardingRuntime::new(pool.clone(), executor, vars, 22, 999).unwrap()
}

fn shared_preview_runtime(
    pool: &PgPool,
    mock: &MockRest,
) -> Arc<crate::command_runtime::CommandRuntime> {
    ensure_crypto_provider();
    let executor = ActionExecutor::with_proxy(
        "welcome-preview-test-only-token".into(),
        Some(mock.origin()),
    )
    .expect("loopback-only shared executor");
    let gates = two_bot_core::RouterGates {
        configured_guild: Some(22),
        scorecard: false,
        automations: false,
        announcements: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: true,
        session_picker: false,
    };
    crate::command_runtime::CommandRuntime::new(
        pool.clone(),
        executor,
        crate::command_runtime::router_with_commands(gates),
        22,
        false,
    )
}

fn closed_pool() -> PgPool {
    PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/postgres")
        .expect("offline lazy pool")
}

fn callbacks(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path.ends_with("/callback"))
        .collect()
}

fn replies(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "PATCH")
        .collect()
}

fn posts(mock: &MockRest) -> Vec<RestRequest> {
    mock.requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path.contains("/channels/"))
        .collect()
}

struct CompletedAdmission;

impl SendCompletion for CompletedAdmission {
    fn complete(
        self: Box<Self>,
        _cooldown: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Blocks exactly the second admission: the ACK lands, the completing edit is
/// refused pre-wire once, then the lane releases for the bounded retry.
#[derive(Debug)]
struct BlockSecondAdmission {
    key: TokenKey,
    attempts: AtomicUsize,
}

impl SendAdmission for BlockSecondAdmission {
    fn token_key(&self) -> &TokenKey {
        &self.key
    }

    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if attempt == 1 {
                Err(AdmissionError::Blocked)
            } else {
                Ok(AdmissionPermit::new(Box::new(CompletedAdmission)))
            }
        })
    }
}

// A transiently held send lane must not fail the preview edit or stop the
// gateway worker: the completing component edit retries pre-wire Blocked,
// then finishes with exactly one wire PATCH.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_preview_blocked_edit_retries_pre_wire_and_completes() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
        ensure_crypto_provider();
        let admission = Arc::new(BlockSecondAdmission {
            key: TokenKey::for_bot_token("test-token").expect("synthetic token key"),
            attempts: AtomicUsize::new(0),
        });
        let executor = ActionExecutor::with_admission(
            "test-token".into(),
            Some(mock.origin()),
            Arc::clone(&admission) as Arc<dyn SendAdmission>,
        )
        .expect("loopback executor with scripted admission");
        let runtime =
            OnboardingRuntime::new(db.pool.clone(), executor, &vars("legacy", false), 22, 999)
                .unwrap();
        runtime
            .handle(job(preview()), NOW)
            .await
            .expect("the retried preview completes without a worker failure");
        assert_eq!(
            admission.attempts.load(Ordering::SeqCst),
            3,
            "ACK plus the blocked edit plus its single retry"
        );
        assert_eq!(callbacks(&mock).len(), 1, "exactly one defer");
        let edits = replies(&mock);
        assert_eq!(
            edits.len(),
            1,
            "exactly one completing PATCH reaches the wire"
        );
        let body: Value = serde_json::from_slice(&edits[0].body).unwrap();
        let content = body["content"].as_str().unwrap_or_default();
        assert!(
            content
                .lines()
                .next()
                .unwrap_or_default()
                .contains("Preview"),
            "the retried edit keeps the preview banner, got {content:?}"
        );
        assert!(
            content.contains(&legacy_welcome_text(765_432_109_876_543_210)),
            "the retried edit keeps the byte-identical legacy body, got {content:?}"
        );
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// A saturated shared lane must yield preview picker clicks before busy
// admission: both consumers together still send exactly one initial callback,
// and the click gets the preview guidance. The same actor's ordinary control
// still reaches the busy reply.
#[tokio::test]
async fn onboarding_preview_click_yields_shared_busy_admission() {
    let pool = closed_pool();
    pool.close().await;
    let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
    let shared = shared_preview_runtime(&pool, &mock);
    let _dispatch_guard = shared.dispatch_guard();
    let _slots: Vec<_> = (0..crate::interaction_admission::PER_USER_IN_FLIGHT)
        .map(|_| {
            shared
                .acquire_user_slot_for_test(44)
                .expect("hold actor slot")
        })
        .collect();
    let click = preview_click("mock-picker-callback", "334", PREVIEW_BANNER_CONTENT);
    assert!(
        crate::onboarding::is_preview_control(&click),
        "the click carries the preview banner fence"
    );
    let event = Event::InteractionCreate(Box::new(InteractionCreate(click.clone())));
    assert!(
        shared.dispatch(&event),
        "saturated shared dispatch yields the preview click"
    );
    assert!(
        mock.requests().is_empty(),
        "the yield sends no competing busy callback"
    );
    let runtime = runtime(&pool, &mock, &vars("legacy", false));
    runtime
        .handle(job(click), NOW)
        .await
        .expect("the owning consumer guides the preview click");
    assert_eq!(
        callbacks(&mock).len(),
        1,
        "only onboarding acknowledges the click"
    );
    let edits = replies(&mock);
    assert_eq!(edits.len(), 1, "the click gets exactly one guidance edit");
    let body: Value = serde_json::from_slice(&edits[0].body).unwrap();
    let content = body["content"].as_str().unwrap_or_default();
    assert!(
        content.contains("not a delivery") && content.contains("No selection was applied"),
        "the click guidance stays read-only, got {content:?}"
    );
    let ordinary = preview_click("mock-busy-control", "335", "ordinary menu without a banner");
    assert!(
        !crate::onboarding::is_preview_control(&ordinary),
        "the control lacks the preview fence"
    );
    let ordinary = Event::InteractionCreate(Box::new(InteractionCreate(ordinary)));
    assert!(
        !shared.dispatch(&ordinary),
        "the ordinary control still reaches busy admission"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if callbacks(&mock).len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the ordinary control receives its busy callback");
    let requests = callbacks(&mock);
    assert_eq!(requests.len(), 2, "defer plus busy, nothing else");
    assert!(
        requests[0].path.contains("/334/mock-picker-callback/"),
        "onboarding deferred the preview click first"
    );
    assert!(requests[1].path.contains("/335/mock-busy-control/"));
    let reply: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(reply["type"], 4);
    assert_eq!(reply["data"]["flags"], 64);
    assert_eq!(
        reply["data"]["content"],
        crate::interaction_admission::BUSY_REPLY
    );
}

// Onboarding resolves the registered command identity before capture, so a
// stale payload name neither silences the preview nor double-answers another
// command, and an obsolete ID cannot spoof the preview name.
#[tokio::test]
async fn onboarding_preview_resolves_registered_identity_before_capture() {
    let pool = closed_pool();
    pool.close().await;
    let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
    ensure_crypto_provider();
    let executor =
        ActionExecutor::with_proxy("onboarding-test-only-token".into(), Some(mock.origin()))
            .unwrap();
    hydrate_preview_identities(&executor);
    let runtime =
        OnboardingRuntime::new(pool.clone(), executor, &vars("legacy", false), 22, 999).unwrap();
    let pipeline = build_pipeline(vec![], None);
    let stale = slash_with_identity(9001, "ping");
    assert!(
        runtime.accepts_interaction(&stale),
        "the stale payload name resolves to the registered preview"
    );
    let event = Event::InteractionCreate(Box::new(InteractionCreate(stale)));
    let captured = runtime
        .capture(&event, &pipeline)
        .expect("the stale preview slash is captured");
    let OnboardingJob::Interaction(boxed) = captured else {
        panic!("capture keeps the interaction job");
    };
    let name = match boxed.data.as_ref() {
        Some(InteractionData::ApplicationCommand(data)) => data.name.clone(),
        other => panic!("canonical capture keeps the slash data, got {other:?}"),
    };
    assert_eq!(
        name, "welcome-preview",
        "capture retains the canonical name for execution"
    );
    let spoof = slash_with_identity(9002, "welcome-preview");
    assert!(
        !runtime.accepts_interaction(&spoof),
        "the registered help ID wins over the raw preview name"
    );
    let obsolete = slash_with_identity(9999, "welcome-preview");
    assert!(
        !runtime.accepts_interaction(&obsolete),
        "an obsolete ID cannot claim the preview name"
    );
    assert!(
        mock.requests().is_empty(),
        "identity checks read no Discord state"
    );
}

// A role-bearing invoker still sees the roleless synthetic newcomer: the
// preview renders byte-identical components with zero preselected options.
// Substituting the invoker's roles for the fixed empty set must fail this.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; never live Discord or DATABASE_URL"]
async fn onboarding_preview_roleless_render_ignores_invoker_roles() {
    let db = TestSchema::new().await;
    let result = std::panic::AssertUnwindSafe(async {
        let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
        let runtime = runtime(&db.pool, &mock, &vars("legacy", false));
        let interaction = preview_slash(admin_member("44", Some("32"), &[GAME_PICKS[0].role_id]));
        assert!(
            runtime.accepts_interaction(&interaction),
            "a role-bearing admin still captures the preview"
        );
        runtime
            .handle(job(interaction), NOW)
            .await
            .expect("the role-bearing preview renders");
        let edits = replies(&mock);
        assert_eq!(edits.len(), 1, "exactly one completing edit");
        let body: Value = serde_json::from_slice(&edits[0].body).unwrap();
        let expected = picker_components(Some(PickerKind::Games), &[], &[]);
        assert_eq!(
            body["components"],
            serde_json::to_value(expected).unwrap(),
            "invoker roles never preselect preview options"
        );
        let menu = &body["components"][0]["components"][0];
        assert_eq!(
            menu["options"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|option| option["default"] == true)
                .count(),
            0,
            "a roleless fixture preselects nothing"
        );
        assert!(posts(&mock).is_empty(), "a preview posts no welcome");
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(events, 0, "a preview records no funnel events");
    })
    .catch_unwind()
    .await;
    db.close().await;
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// Authorization precedes settings: a non-admin gets the exact shared refusal
// even when configuration is unavailable (closed pool), never the
// configuration-failure guidance. Loading configuration first must fail this.
#[tokio::test]
async fn onboarding_preview_refusal_precedes_unavailable_settings() {
    for permissions in [Some("0"), None] {
        let pool = closed_pool();
        pool.close().await;
        let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState::default()))).await;
        let runtime = runtime(&pool, &mock, &vars("legacy", false));
        let interaction = preview_slash(admin_member("44", permissions, &[]));
        runtime
            .handle(job(interaction), NOW)
            .await
            .expect("the refusal completes without settings");
        assert_eq!(callbacks(&mock).len(), 1, "one defer, never retried");
        let edits = replies(&mock);
        assert_eq!(edits.len(), 1, "one refusal edit");
        let body: Value = serde_json::from_slice(&edits[0].body).unwrap();
        let content = body["content"].as_str().unwrap_or_default();
        assert_eq!(
            content,
            two_bot_core::RouterRefusal::ManageServerRequired.message(),
            "unavailable settings cannot change the refusal"
        );
    }
}

// A failed preview ACK renders the preview-specific guidance (banner plus
// no-delivery wording), never the generic already-processed fallback.
// Reverting the branch to the generic reply must fail this.
#[tokio::test]
async fn onboarding_preview_unconfirmed_ack_states_no_delivery_attempted() {
    let pool = closed_pool();
    pool.close().await;
    let mock = preview_discord(Arc::new(Mutex::new(PreviewDiscordState {
        reject_callback: true,
    })))
    .await;
    let runtime = runtime(&pool, &mock, &vars("legacy", false));
    runtime
        .handle(job(preview()), NOW)
        .await
        .expect("the unconfirmed preview guides privately");
    assert_eq!(callbacks(&mock).len(), 1, "the failed ACK is never retried");
    let edits = replies(&mock);
    assert_eq!(edits.len(), 1, "one guidance edit");
    let body: Value = serde_json::from_slice(&edits[0].body).unwrap();
    let content = body["content"].as_str().unwrap_or_default();
    let first = content.lines().next().unwrap_or_default();
    assert!(
        first.to_lowercase().contains("preview"),
        "the guidance keeps the preview banner, got {first:?}"
    );
    assert!(
        content.contains("No delivery was attempted"),
        "the guidance states no delivery was attempted, got {content:?}"
    );
    assert!(
        !content.contains("may already have been processed"),
        "no generic already-processed fallback, got {content:?}"
    );
}
