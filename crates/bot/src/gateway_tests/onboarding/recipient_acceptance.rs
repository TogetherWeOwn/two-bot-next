#![cfg(test)]
//! Protected acceptance: queued welcomes must use current recipient evidence.
//!
//! The Builder must keep this file and recipient_recovery_acceptance.rs intact;
//! corrections belong to the test author. Only local HTTP/WebSocket doubles and
//! a guarded disposable database are used. Captured eligibility may be narrowed,
//! never widened, by a fresh member read. The read must precede the prompt lock.

use std::sync::Mutex;

use two_bot_core::onboarding::{PLATFORM_PICKS, SESSION_SELECT_ID};
use two_bot_core::onboarding_store::{begin_prompt, has_onboarding_prompt};
use two_bot_testsupport::TestDatabase;

use super::*;

mod recipient_recovery_acceptance;

// CI already creates this bootstrap in the runtime lane. Other lanes/local
// callers can supply their guarded two_bot_test_* bootstrap explicitly. There
// is no connection retry with another URL or credential after setup failure.
const CI_BOOTSTRAP: &str = "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci";
const RECIPIENT_PATH: &str = "/api/v10/guilds/2222/members/77";
const PROMPT_PATH: &str = "/api/v10/channels/12/messages";
const SLOW_RESPONSE: Duration = Duration::from_secs(60);

// The existing CI onboarding step runs in parallel. Full-migration disposable
// databases are larger than its older schema fixtures: serialize this suite's
// owned databases, rather than filling the service's bounded tmpfs.
static DATABASE_FENCE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Case {
    db: TestDatabase,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl Case {
    async fn new() -> Self {
        let serial = DATABASE_FENCE.lock().await;
        let url = std::env::var("TWO_TEST_DATABASE_URL").unwrap_or_else(|_| CI_BOOTSTRAP.into());
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("guarded disposable recipient fixture");
        Self {
            db,
            _serial: serial,
        }
    }

    fn pool(&self) -> &sqlx::PgPool {
        self.db.pool()
    }

    fn store(&self) -> GatewaySessionStore {
        GatewaySessionStore::new(self.pool().clone(), GUILD.into(), 0)
    }

    async fn prompts(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM events WHERE event_type = 'onboarding_prompted'")
            .fetch_one(self.pool())
            .await
            .expect("prompt row count")
    }

    async fn assert_unsent(&self, mock: &MockRest) {
        assert!(
            posts(mock).is_empty(),
            "no welcome POST for unsafe evidence"
        );
        assert_eq!(self.prompts().await, 0, "no success prompt row");
        assert!(!has_onboarding_prompt(self.pool(), GUILD, "77")
            .await
            .expect("prompt marker read"));
        let routes: i64 =
            sqlx::query_scalar("SELECT count(*) FROM events WHERE event_type = 'channel_routed'")
                .fetch_one(self.pool())
                .await
                .expect("route row count");
        assert_eq!(routes, 0, "anchor must not record a false success route");
    }
}

async fn finish(case: Case, mock: MockRest, result: std::thread::Result<()>) {
    mock.shutdown().await;
    case.db
        .close()
        .await
        .expect("drop only the owned disposable database");
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

#[derive(Clone, Copy, Debug)]
enum Evidence {
    Valid,
    ChangedRoles,
    OmittedBot,
    OmittedPending,
    OmittedBoth,
    Absent,
    Pending,
    Bot,
    WrongIdentity,
    MissingUser,
    MalformedPending,
    MalformedBot,
    MissingRoles,
    NonArrayRoles,
    MalformedRoleId,
    NumericRoleId,
    Forbidden,
    Unavailable,
    Timeout,
}

fn member() -> Value {
    json!({
        "user":{"id":"77","username":"fixture-member","discriminator":"0","bot":false},
        "roles":[GAME_PICKS[0].role_id], "pending":false,
        "deaf":false,"mute":false,"flags":0,
        "joined_at":"2026-09-29T12:00:00.000000+00:00"
    })
}

fn recipient_response(evidence: Evidence) -> ScriptedResponse {
    let mut body = member();
    match evidence {
        Evidence::Valid => {}
        Evidence::ChangedRoles => {
            body["roles"] = json!([GAME_PICKS[1].role_id, PLATFORM_PICKS[0].role_id])
        }
        Evidence::OmittedBot | Evidence::OmittedPending | Evidence::OmittedBoth => {
            body["roles"] = json!([GAME_PICKS[1].role_id, PLATFORM_PICKS[0].role_id]);
            if matches!(evidence, Evidence::OmittedBot | Evidence::OmittedBoth) {
                body["user"].as_object_mut().unwrap().remove("bot");
            }
            if matches!(evidence, Evidence::OmittedPending | Evidence::OmittedBoth) {
                body.as_object_mut().unwrap().remove("pending");
            }
        }
        Evidence::Absent => {
            return ScriptedResponse::json(404, json!({"code":10007,"message":"Unknown Member"}))
        }
        Evidence::Pending => body["pending"] = json!(true),
        Evidence::Bot => body["user"]["bot"] = json!(true),
        Evidence::WrongIdentity => body["user"]["id"] = json!("78"),
        Evidence::MissingUser => {
            body.as_object_mut().unwrap().remove("user");
        }
        Evidence::MalformedPending => body["pending"] = json!("false"),
        Evidence::MalformedBot => body["user"]["bot"] = json!("false"),
        Evidence::MissingRoles => {
            body.as_object_mut().unwrap().remove("roles");
        }
        Evidence::NonArrayRoles => body["roles"] = json!("not-an-array"),
        Evidence::MalformedRoleId => body["roles"] = json!(["not-a-snowflake"]),
        Evidence::NumericRoleId => body["roles"] = json!([55]),
        Evidence::Forbidden => return ScriptedResponse::status(403),
        Evidence::Unavailable => return ScriptedResponse::status(503),
        Evidence::Timeout => return ScriptedResponse::json(200, body).delayed(SLOW_RESPONSE),
    }
    ScriptedResponse::json(200, body)
}

struct Discord {
    evidence: Evidence,
    deny_bot: bool,
    deny_channel: bool,
}

async fn recipient_discord(evidence: Evidence) -> (MockRest, Arc<Mutex<Discord>>) {
    let state = Arc::new(Mutex::new(Discord {
        evidence,
        deny_bot: false,
        deny_channel: false,
    }));
    let responder = state.clone();
    let mock = MockRest::with_responder(move |request| {
        let state = responder.lock().unwrap();
        let path = request.path.strip_prefix("/api/v10").expect("mock API prefix");
        match (request.method.as_str(), path) {
            ("GET", "/guilds/2222/members/77") => recipient_response(state.evidence),
            ("GET", "/guilds/2222/members/999") if state.deny_bot => ScriptedResponse::status(403),
            ("GET", "/guilds/2222/members/999") => ScriptedResponse::json(200, json!({"user":{"id":"999","bot":true},"roles":[],"pending":false})),
            ("GET", "/guilds/2222") => ScriptedResponse::json(200, json!({"id":GUILD,"owner_id":"888"})),
            ("GET", "/guilds/2222/roles") => {
                let mut roles = vec![json!({"id":GUILD,"permissions":"1051648"})];
                roles.extend(GAME_PICKS.iter().chain(PLATFORM_PICKS.iter()).map(|pick| json!({"id":pick.role_id,"permissions":"0"})));
                ScriptedResponse::json(200, json!(roles))
            }
            ("GET", "/channels/10" | "/channels/11" | "/channels/12" | "/channels/14") => {
                let id = path.strip_prefix("/channels/").unwrap();
                let overwrites = if state.deny_channel { json!([{"id":"999","type":1,"allow":"0","deny":"3072"}]) } else { json!([]) };
                ScriptedResponse::json(200, json!({"id":id,"guild_id":GUILD,"type":if matches!(id,"11"|"14") {2} else {0},"permission_overwrites":overwrites}))
            }
            ("POST", "/channels/12/messages" | "/channels/14/messages") => ScriptedResponse::json(201, json!({"id":"99"})).delayed(Duration::from_millis(25)),
            _ => ScriptedResponse::status(404),
        }
    }).await;
    (mock, state)
}

fn runtime_for(
    pool: &sqlx::PgPool,
    mock: &MockRest,
    mode: &str,
    dry_run: bool,
) -> OnboardingRuntime {
    ensure_crypto_provider();
    let vars = HashMap::from([
        ("DISCORD_GUILD_ID".into(), GUILD.into()),
        ("TWO_ONBOARDING_MODE".into(), mode.into()),
        (
            "TWO_ONBOARDING_DRY_RUN".into(),
            if dry_run { "1" } else { "0" }.into(),
        ),
        ("DISCORD_LANDING_CHANNEL_IDS".into(), "12".into()),
        ("DISCORD_ANCHOR_WELCOME_CHANNEL_ID".into(), "14".into()),
        (
            "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID".into(),
            "10".into(),
        ),
        ("DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID".into(), "11".into()),
    ]);
    OnboardingRuntime::new(
        pool.clone(),
        ActionExecutor::with_proxy(TOKEN.into(), Some(mock.origin())).unwrap(),
        &vars,
        2222,
        999,
    )
    .unwrap()
}

fn queued_welcome() -> OnboardingJob {
    OnboardingJob::Welcome {
        guild_id: 2222,
        member_id: 77,
        bot: false,
        pending: false,
        trigger: MembershipTrigger::Joined { pending: false },
        roles: vec![GAME_PICKS[0].role_id.to_owned()],
    }
}

fn assert_welcome(mock: &MockRest, mode: &str) -> Value {
    let messages = posts(mock);
    assert_eq!(messages.len(), 1, "one accepted welcome");
    assert_eq!(
        messages[0].path,
        if mode == "anchor" {
            "/api/v10/channels/14/messages"
        } else {
            PROMPT_PATH
        }
    );
    let body: Value = serde_json::from_slice(&messages[0].body).expect("welcome JSON");
    assert_eq!(
        body["allowed_mentions"],
        json!({"parse":[],"users":["77"],"roles":[],"replied_user":false})
    );
    assert!(
        !mock
            .requests()
            .iter()
            .any(|request| matches!(request.method.as_str(), "PUT" | "PATCH" | "DELETE")),
        "welcome must not mutate member roles"
    );
    if mode == "anchor" {
        assert!(body.get("components").is_none());
        assert!(body.get("embeds").is_none());
    } else {
        assert_eq!(
            body["components"][0]["components"][0]["custom_id"],
            if mode == "session" {
                SESSION_SELECT_ID
            } else {
                GAME_SELECT_ID
            }
        );
    }
    body
}

fn assert_current_defaults(body: &Value) {
    let menu = &body["components"][0]["components"][0];
    assert_eq!(menu["min_values"], 0);
    assert_eq!(menu["max_values"], 10);
    let options = menu["options"].as_array().expect("game options");
    let actual: Vec<(&str, bool)> = options
        .iter()
        .map(|option| {
            (
                option["value"].as_str().unwrap(),
                option["default"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    assert_eq!(
        actual,
        vec![
            ("shooters", false),
            ("survival", true),
            ("horror", false),
            ("counterstrike", false),
            ("rocketleague", false),
            ("fallguys", false),
            ("warthunder", false),
            ("retro", false),
            ("tabletop", false),
            ("pokemon", false),
        ],
        "current survival role replaces captured shooters; platform roles are not game defaults"
    );
}

async fn terminal_recipient(evidence: Evidence) {
    for mode in ["legacy", "session", "anchor"] {
        let case = Case::new().await;
        let (mock, state) = recipient_discord(evidence).await;
        let result = bounded(async {
            let runtime = runtime_for(case.pool(), &mock, mode, false);
            case.assert_unsent(&mock).await;
            assert!(
                runtime.handle(queued_welcome(), RECEIPT_AT).await.is_ok(),
                "proven ineligibility is an intentional no-op"
            );
            case.assert_unsent(&mock).await;
            state.lock().unwrap().evidence = Evidence::Valid;
            let mut later = queued_welcome();
            if let OnboardingJob::Welcome { trigger, .. } = &mut later {
                *trigger = MembershipTrigger::GateCleared;
            }
            runtime
                .handle(later, RECEIPT_AT + 1)
                .await
                .expect("later eligible gate-clear is not suppressed");
            assert_welcome(&mock, mode);
            assert_eq!(case.prompts().await, 1);
        })
        .await;
        finish(case, mock, result).await;
    }
}

async fn retryable_recipient(evidence: Evidence) {
    let case = Case::new().await;
    let (mock, state) = recipient_discord(evidence).await;
    let result = bounded(async {
        let runtime = runtime_for(case.pool(), &mock, "legacy", false);
        case.assert_unsent(&mock).await;
        let outcome = tokio::time::timeout(
            Duration::from_secs(15),
            runtime.handle(queued_welcome(), RECEIPT_AT),
        )
        .await
        .expect(
            "recipient read must have its own finite deadline, below the worker's outer budget",
        );
        assert!(
            outcome.is_err(),
            "{evidence:?} is retryable unavailable evidence, not terminal absence"
        );
        case.assert_unsent(&mock).await;
        state.lock().unwrap().evidence = Evidence::Valid;
        runtime
            .handle(queued_welcome(), RECEIPT_AT + 1)
            .await
            .expect("fresh evidence permits retry");
        assert_welcome(&mock, "legacy");
        assert_eq!(case.prompts().await, 1);
    })
    .await;
    finish(case, mock, result).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_absent_does_not_mark_or_suppress_a_later_join() {
    terminal_recipient(Evidence::Absent).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_pending_does_not_mark_or_suppress_a_later_gate_clear() {
    terminal_recipient(Evidence::Pending).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_now_bot_does_not_mark_or_suppress_a_later_valid_opportunity() {
    terminal_recipient(Evidence::Bot).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_wrong_identity_is_retryable() {
    retryable_recipient(Evidence::WrongIdentity).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_missing_user_is_retryable() {
    retryable_recipient(Evidence::MissingUser).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_malformed_pending_is_retryable() {
    retryable_recipient(Evidence::MalformedPending).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_malformed_bot_is_retryable() {
    retryable_recipient(Evidence::MalformedBot).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_missing_roles_is_retryable() {
    retryable_recipient(Evidence::MissingRoles).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_nonarray_roles_are_retryable() {
    retryable_recipient(Evidence::NonArrayRoles).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_malformed_role_id_is_retryable() {
    retryable_recipient(Evidence::MalformedRoleId).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_numeric_role_id_is_retryable() {
    retryable_recipient(Evidence::NumericRoleId).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_forbidden_is_not_absence() {
    retryable_recipient(Evidence::Forbidden).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_unavailable_is_retryable() {
    retryable_recipient(Evidence::Unavailable).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_timeout_is_bounded_and_retryable() {
    retryable_recipient(Evidence::Timeout).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_current_roles_replace_captured_menu_defaults() {
    let case = Case::new().await;
    let (mock, _) = recipient_discord(Evidence::ChangedRoles).await;
    let result = bounded(async {
        let runtime = runtime_for(case.pool(), &mock, "legacy", false);
        case.assert_unsent(&mock).await;
        runtime
            .handle(queued_welcome(), RECEIPT_AT)
            .await
            .expect("current valid member");
        assert_current_defaults(&assert_welcome(&mock, "legacy"));
        assert_eq!(case.prompts().await, 1);
    })
    .await;
    finish(case, mock, result).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_lookup_precedes_prompt_guard_acquisition() {
    let case = Case::new().await;
    let (mock, _) = recipient_discord(Evidence::Absent).await;
    let result = bounded(async {
        let runtime = runtime_for(case.pool(), &mock, "legacy", false);
        let held = begin_prompt(case.pool(), GUILD, "77")
            .await
            .unwrap()
            .expect("hold the real member lock");
        let handled = tokio::time::timeout(
            Duration::from_secs(3),
            runtime.handle(queued_welcome(), RECEIPT_AT),
        )
        .await;
        drop(held);
        assert!(handled
            .expect("an absent recipient must finish without waiting for the send guard")
            .is_ok());
        assert!(mock
            .requests()
            .iter()
            .any(|request| request.method == "GET" && request.path == RECIPIENT_PATH));
        case.assert_unsent(&mock).await;
    })
    .await;
    finish(case, mock, result).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_valid_concurrent_and_redelivered_controls_send_once() {
    let case = Case::new().await;
    let (mock, _) = recipient_discord(Evidence::Valid).await;
    let peer = case
        .db
        .independent_pool()
        .await
        .expect("independent process pool");
    let result = bounded(async {
        let first = runtime_for(case.pool(), &mock, "legacy", false);
        let second = runtime_for(&peer, &mock, "legacy", false);
        case.assert_unsent(&mock).await;
        let calls = (0..12).map(|index| {
            let runtime = if index % 2 == 0 { &first } else { &second };
            async move { runtime.handle(queued_welcome(), RECEIPT_AT).await }
        });
        for outcome in futures_util::future::join_all(calls).await {
            outcome.expect("concurrent valid welcome");
        }
        assert_welcome(&mock, "legacy");
        assert_eq!(case.prompts().await, 1);
        first
            .handle(queued_welcome(), RECEIPT_AT + 1)
            .await
            .unwrap();
        assert_eq!(posts(&mock).len(), 1, "committed marker fences redelivery");
        assert!(has_onboarding_prompt(case.pool(), GUILD, "77")
            .await
            .unwrap());
    })
    .await;
    peer.close().await;
    finish(case, mock, result).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_fresh_valid_state_never_admits_an_ineligible_captured_trigger() {
    let case = Case::new().await;
    let (mock, _) = recipient_discord(Evidence::Valid).await;
    let result = bounded(async {
        let runtime = runtime_for(case.pool(), &mock, "legacy", false);
        for (guild_id, bot, pending, trigger) in [
            (2223, false, false, MembershipTrigger::GateCleared),
            (2222, true, false, MembershipTrigger::GateCleared),
            (2222, false, true, MembershipTrigger::GateCleared),
            (
                2222,
                false,
                false,
                MembershipTrigger::Joined { pending: true },
            ),
        ] {
            runtime
                .handle(
                    OnboardingJob::Welcome {
                        guild_id,
                        member_id: 77,
                        bot,
                        pending,
                        trigger,
                        roles: vec![],
                    },
                    RECEIPT_AT,
                )
                .await
                .unwrap();
            case.assert_unsent(&mock).await;
        }
        assert!(
            mock.requests().is_empty(),
            "fresh evidence cannot invent eligibility"
        );
    })
    .await;
    finish(case, mock, result).await;
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_valid_state_preserves_mode_and_dry_run_controls() {
    for (mode, dry_run, sent) in [
        ("legacy", true, false),
        ("anchor", true, false),
        ("session", true, true),
        ("anchor", false, true),
        ("session", false, true),
    ] {
        let case = Case::new().await;
        let (mock, _) = recipient_discord(Evidence::Valid).await;
        let result = bounded(async {
            runtime_for(case.pool(), &mock, mode, dry_run)
                .handle(queued_welcome(), RECEIPT_AT)
                .await
                .unwrap();
            if sent {
                assert_welcome(&mock, mode);
                assert_eq!(case.prompts().await, 1);
            } else {
                case.assert_unsent(&mock).await;
            }
        })
        .await;
        finish(case, mock, result).await;
    }
}

#[tokio::test]
#[ignore = "requires guarded disposable agent-testdb; local mock Discord only"]
async fn onboarding_recipient_valid_state_preserves_live_bot_and_channel_permission_controls() {
    for deny_bot in [true, false] {
        let case = Case::new().await;
        let (mock, state) = recipient_discord(Evidence::Valid).await;
        {
            let mut state = state.lock().unwrap();
            state.deny_bot = deny_bot;
            state.deny_channel = !deny_bot;
        }
        let result = bounded(async {
            let outcome = runtime_for(case.pool(), &mock, "legacy", false)
                .handle(queued_welcome(), RECEIPT_AT)
                .await;
            assert!(
                outcome.is_ok(),
                "existing proven bot/channel denial remains a terminal no-op"
            );
            case.assert_unsent(&mock).await;
        })
        .await;
        finish(case, mock, result).await;
    }
}
