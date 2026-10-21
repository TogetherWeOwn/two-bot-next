//! Admin API dry-run + confirm execution acceptance.
//!
//! Failing acceptance for the dry-run + confirm execution slice. Every test
//! marked RED fails on the baseline (no dry-run or confirm path exists) and
//! passes once the slice lands. Every test marked CONTROL passes on the
//! baseline and must keep passing after the slice lands: it pins behavior the
//! slice must preserve.
//!
//! Contract under test:
//!
//! - `dry_run: true` in a dispatch body returns a plan with zero side
//!   effects: zero Discord calls and zero terminal/idempotency rows. The plan
//!   carries the param-validation result plus the would-do diff.
//! - Destructive verbs (`moderation.ban`, `moderation.tempban`,
//!   `moderation.purge`, `moderation.lockdown`) require a confirm that
//!   executes exactly once through the existing idempotency claim/finish
//!   path. Replaying a confirm under a new idempotency key is refused, while
//!   retrying under the same key still replays the stored outcome (the replay
//!   check runs before the confirm-reuse check).
//! - Dry-run plans for raid lockdown and mass purge name the break-glass
//!   lever (`/ops/kill-switch`).
//! - `moderation.warn`, `moderation.timeout`, `moderation.kick` and
//!   `moderation.slowmode` stay direct-with-audit (no confirm); reads never
//!   require confirm.
//!
//! Builder contract: land these exact inherent items (the adapter traits
//! below provide baseline behavior so this file compiles pre-slice; inherent
//! items take precedence at every unqualified call site post-slice, and the
//! CONTROL tests with explicit qualified calls keep the traits live so no
//! dead-code warning can fail the build):
//!
//! ```ignore
//! impl AuthDecision {
//!     pub fn dry_run_requested(&self) -> bool;
//!     pub fn dry_run_plan(&self) -> serde_json::Value;
//!     pub fn requires_confirm(&self) -> bool;
//! }
//! impl<D, S, F> MemberModerationService<D, S, F>
//! where
//!     D: MemberDiscord,
//!     S: MemberModerationStore,
//!     F: Fn() -> i64 + Send + Sync,
//! {
//!     pub async fn execute_dry_run(&self, exec: &MemberExecution) -> serde_json::Value;
//!     pub async fn execute_confirmed(
//!         &self,
//!         exec: &MemberExecution,
//!         confirm_id: &str,
//!     ) -> Result<MemberResult, MemberError>;
//! }
//! ```
//!
//! Plan shape (JSON object): `dry_run: true`, `verb` (the action string),
//! `valid` (param validation passed), `confirm_required` (code-list lookup),
//! `would_do` (non-empty object when valid), plus `confirm_single_use: true`
//! and `confirm_ttl_secs: 300` when a confirm is required, plus
//! `break_glass: "/ops/kill-switch"` for lockdown and purge plans. A refused
//! confirm may surface as any `MemberError`; the tests assert refusal plus
//! zero side effects, never the variant.
//!
//! Fakes only: synthetic snowflakes, `MockMemberDiscord` (call counting) and
//! `MemMemberStore` (audit rows). No database, no network, no Discord.

use serde_json::{json, Value};
use two_bot_core::commands::{PERM_BAN_MEMBERS, PERM_KICK_MEMBERS, PERM_MODERATE_MEMBERS};
use two_bot_core::internal_actions::AuthDecision;
use two_bot_core::member_moderation::{
    DiscordCall, MemMemberStore, MemberError, MemberExecution, MemberModerationService,
    MemberOutcome, MemberResult, MockMemberDiscord,
};
use two_bot_core::moderation::{
    ModerationAction, ModerationActor, ModerationPolicy, ModerationTarget,
};

const GUILD_ID: &str = "100000000000000001";
const ACTOR_ID: &str = "111111111111111111";
const TARGET_ID: &str = "333333333333333333";
const CHANNEL_ID: &str = "666666666666666666";
const STAFF_ROLE_ID: &str = "444444444444444444";

const BREAK_GLASS_LEVER: &str = "/ops/kill-switch";

type TestService = MemberModerationService<MockMemberDiscord, MemMemberStore, fn() -> i64>;

// --- Baseline adapters (builder overrides with inherent items) ---

/// Pre-slice, dispatch has no dry-run flag: nothing requests a dry run.
trait AdminDryRunFlag {
    fn dry_run_requested(&self) -> bool;
}

impl AdminDryRunFlag for AuthDecision {
    fn dry_run_requested(&self) -> bool {
        false
    }
}

/// Pre-slice, dispatch builds no plan.
trait AdminDryRunPlan {
    fn dry_run_plan(&self) -> Value;
}

impl AdminDryRunPlan for AuthDecision {
    fn dry_run_plan(&self) -> Value {
        Value::Null
    }
}

/// Pre-slice, no verb requires a confirm.
trait AdminConfirmGate {
    fn requires_confirm(&self) -> bool;
}

impl AdminConfirmGate for AuthDecision {
    fn requires_confirm(&self) -> bool {
        false
    }
}

/// Pre-slice, the closest behavior to a dry run is a real execution: the
/// purity tests fail because Discord is called and rows are written.
trait AdminDryRunExecute {
    async fn execute_dry_run(&self, exec: &MemberExecution) -> Value;
}

impl AdminDryRunExecute for TestService {
    async fn execute_dry_run(&self, exec: &MemberExecution) -> Value {
        let _baseline_executes = self.execute(exec).await;
        Value::Null
    }
}

/// Pre-slice, confirms do not exist: the id is ignored and the request
/// executes under its own idempotency key.
trait AdminConfirmExecute {
    async fn execute_confirmed(
        &self,
        exec: &MemberExecution,
        confirm_id: &str,
    ) -> Result<MemberResult, MemberError>;
}

impl AdminConfirmExecute for TestService {
    async fn execute_confirmed(
        &self,
        exec: &MemberExecution,
        _confirm_id: &str,
    ) -> Result<MemberResult, MemberError> {
        self.execute(exec).await
    }
}

// --- Fixtures ---

fn policy() -> ModerationPolicy {
    ModerationPolicy {
        owen_user_id: "123456789012345678".to_owned(),
        protected_role_ids: std::collections::HashSet::from([STAFF_ROLE_ID.to_owned()]),
        bot_user_id: Some("555555555555555555".to_owned()),
    }
}

fn actor_with(permissions: u64, position: i64) -> ModerationActor {
    ModerationActor {
        user_id: ACTOR_ID.to_owned(),
        role_ids: vec![],
        highest_role_position: position,
        permissions,
    }
}

fn plain_target(position: i64) -> ModerationTarget {
    ModerationTarget {
        user_id: TARGET_ID.to_owned(),
        role_ids: vec![],
        highest_role_position: position,
        is_bot: false,
        is_guild_owner: false,
    }
}

fn service_with(discord: MockMemberDiscord, store: MemMemberStore) -> TestService {
    MemberModerationService::new(discord, store, policy(), || 1_700_000_000_000)
}

fn ban_execution_keyed(key: &str) -> MemberExecution {
    MemberExecution {
        action: ModerationAction::Ban,
        guild_id: GUILD_ID.to_owned(),
        actor: actor_with(PERM_BAN_MEMBERS, 50),
        target: Some(plain_target(10)),
        bot_highest_role_position: 100,
        reason: "spam in #general".to_owned(),
        duration_seconds: None,
        request_id: key.to_owned(),
        idempotency_key: key.to_owned(),
    }
}

fn warn_execution_keyed(key: &str) -> MemberExecution {
    MemberExecution {
        action: ModerationAction::Warn,
        guild_id: GUILD_ID.to_owned(),
        actor: actor_with(PERM_MODERATE_MEMBERS, 50),
        target: Some(plain_target(10)),
        bot_highest_role_position: 100,
        reason: "repeated spam in #general".to_owned(),
        duration_seconds: None,
        request_id: key.to_owned(),
        idempotency_key: key.to_owned(),
    }
}

fn kick_execution() -> MemberExecution {
    MemberExecution {
        action: ModerationAction::Kick,
        guild_id: GUILD_ID.to_owned(),
        actor: actor_with(PERM_KICK_MEMBERS, 50),
        target: Some(plain_target(10)),
        bot_highest_role_position: 100,
        reason: "spam in #general".to_owned(),
        duration_seconds: None,
        request_id: "req-kick".to_owned(),
        idempotency_key: "req-kick".to_owned(),
    }
}

fn decision(action: &str, body: Value) -> AuthDecision {
    AuthDecision {
        key_id: "test-key".to_owned(),
        action: action.to_owned(),
        body: body.as_object().cloned().expect("test body is an object"),
    }
}

fn member_body(dry_run: bool) -> Value {
    let mut body = json!({
        "actor_id": ACTOR_ID,
        "discord_id": TARGET_ID,
        "reason": "spam in #general",
    });
    if dry_run {
        body["dry_run"] = json!(true);
    }
    body
}

fn tempban_body() -> Value {
    json!({
        "actor_id": ACTOR_ID,
        "discord_id": TARGET_ID,
        "reason": "raid spam",
        "duration_seconds": 3600,
        "dry_run": true,
    })
}

fn timeout_body() -> Value {
    json!({
        "actor_id": ACTOR_ID,
        "discord_id": TARGET_ID,
        "reason": "cooldown",
        "duration_seconds": 3600,
    })
}

fn channel_body(extra: Value) -> Value {
    let mut body = json!({
        "actor_id": ACTOR_ID,
        "channel_id": CHANNEL_ID,
        "reason": "raid",
        "dry_run": true,
    });
    for (key, value) in extra.as_object().expect("extra fields are an object") {
        body[key] = value.clone();
    }
    body
}

// --- dry_run flag in dispatch ---

#[test]
fn red_dry_run_flag_true_is_visible_in_dispatch_decision() {
    let d = decision("moderation.ban", member_body(true));
    assert!(
        d.dry_run_requested(),
        "dry_run:true must be visible to dispatch"
    );
}

#[test]
fn control_dry_run_flag_defaults_to_false() {
    let absent = decision("moderation.ban", member_body(false));
    assert!(!absent.dry_run_requested());
    let mut explicit = member_body(false);
    explicit["dry_run"] = json!(false);
    assert!(!decision("moderation.ban", explicit).dry_run_requested());
}

#[test]
fn control_baseline_dry_run_flag_adapter_stays_live() {
    // Explicit qualified call: keeps the adapter trait used after the slice
    // lands its inherent item, so no dead-code warning can fail the build.
    let d = decision("moderation.ban", member_body(false));
    assert!(!<AuthDecision as AdminDryRunFlag>::dry_run_requested(&d));
}

// --- Plan builder ---

#[test]
fn red_dry_run_plan_describes_ban_with_confirm_mechanics() {
    let plan = decision("moderation.ban", member_body(true)).dry_run_plan();
    assert_eq!(plan["dry_run"], json!(true));
    assert_eq!(plan["verb"], json!("moderation.ban"));
    assert_eq!(plan["valid"], json!(true));
    assert_eq!(plan["confirm_required"], json!(true));
    assert!(
        plan["would_do"].as_object().is_some_and(|o| !o.is_empty()),
        "ban plan must carry a non-empty would-do diff, got {plan}"
    );
    assert_eq!(plan["confirm_single_use"], json!(true));
    assert_eq!(plan["confirm_ttl_secs"], json!(300));
}

#[test]
fn red_dry_run_plan_reports_invalid_params_without_executing() {
    let mut body = member_body(true);
    body["discord_id"] = json!("not-a-snowflake");
    let plan = decision("moderation.ban", body).dry_run_plan();
    assert_eq!(
        plan["valid"],
        json!(false),
        "plan must report param validation, got {plan}"
    );
    assert_eq!(plan["dry_run"], json!(true));
}

#[test]
fn red_dry_run_plan_names_break_glass_for_lockdown() {
    let plan = decision("moderation.lockdown", channel_body(json!({}))).dry_run_plan();
    assert_eq!(plan["valid"], json!(true));
    assert_eq!(plan["confirm_required"], json!(true));
    assert_eq!(
        plan["break_glass"],
        json!(BREAK_GLASS_LEVER),
        "raid-lockdown plan must name the break-glass lever, got {plan}"
    );
}

#[test]
fn red_dry_run_plan_names_break_glass_for_mass_purge() {
    let plan = decision("moderation.purge", channel_body(json!({ "count": 50 }))).dry_run_plan();
    assert_eq!(plan["valid"], json!(true));
    assert_eq!(plan["confirm_required"], json!(true));
    assert_eq!(
        plan["break_glass"],
        json!(BREAK_GLASS_LEVER),
        "mass-purge plan must name the break-glass lever, got {plan}"
    );
}

#[test]
fn red_dry_run_plan_marks_warn_confirm_free() {
    let plan = decision("moderation.warn", member_body(true)).dry_run_plan();
    assert_eq!(plan["dry_run"], json!(true));
    assert_eq!(plan["valid"], json!(true));
    assert_eq!(
        plan["confirm_required"],
        json!(false),
        "reversible warn plans never require confirm, got {plan}"
    );
}

#[test]
fn control_baseline_plan_adapter_returns_null() {
    let d = decision("moderation.ban", member_body(true));
    assert_eq!(
        <AuthDecision as AdminDryRunPlan>::dry_run_plan(&d),
        Value::Null
    );
}

// --- Confirm gate: destructive list vs direct verbs vs reads ---

#[test]
fn red_destructive_verbs_require_confirm() {
    for (action, body) in [
        ("moderation.ban", member_body(true)),
        ("moderation.tempban", tempban_body()),
        ("moderation.purge", channel_body(json!({ "count": 50 }))),
        ("moderation.lockdown", channel_body(json!({}))),
    ] {
        assert!(
            decision(action, body).requires_confirm(),
            "{action} must require a confirm"
        );
    }
}

#[test]
fn control_reversible_verbs_stay_direct_without_confirm() {
    for (action, body) in [
        ("moderation.warn", member_body(false)),
        ("moderation.timeout", timeout_body()),
        ("moderation.kick", member_body(false)),
        (
            "moderation.slowmode",
            channel_body(json!({ "seconds": 30 })),
        ),
    ] {
        assert!(
            !decision(action, body).requires_confirm(),
            "{action} must stay direct-with-audit"
        );
    }
}

#[test]
fn control_reads_never_require_confirm() {
    for (action, body) in [
        ("event.read", json!({ "event_key": "evt_test_001" })),
        ("settings.get", json!({ "key": "test.key" })),
        ("automations.export", json!({})),
    ] {
        assert!(
            !decision(action, body).requires_confirm(),
            "{action} must never require a confirm"
        );
    }
}

#[test]
fn control_baseline_confirm_gate_adapter_stays_live() {
    let d = decision("moderation.ban", member_body(false));
    assert!(!<AuthDecision as AdminConfirmGate>::requires_confirm(&d));
}

// --- Dry-run purity: zero Discord calls, zero terminal rows ---

#[tokio::test]
async fn red_dry_run_ban_plans_with_zero_discord_calls_and_zero_rows() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let plan = svc
        .execute_dry_run(&ban_execution_keyed("req-dry-ban"))
        .await;
    assert_eq!(plan["dry_run"], json!(true), "got {plan}");
    assert!(
        discord.calls().is_empty(),
        "dry run must issue zero Discord calls, got {:?}",
        discord.calls()
    );
    assert!(
        store.audits().is_empty(),
        "dry run must write zero audit/terminal rows, got {:?}",
        store.audits()
    );
}

#[tokio::test]
async fn red_dry_run_does_not_claim_the_idempotency_key() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let _plan = svc
        .execute_dry_run(&ban_execution_keyed("req-dry-key"))
        .await;
    // The same key still executes fresh: the dry run left no terminal row.
    let executed = svc
        .execute(&ban_execution_keyed("req-dry-key"))
        .await
        .expect("key untouched by the dry run executes");
    assert_eq!(executed.outcome, MemberOutcome::Banned);
    assert!(
        !executed.replayed,
        "dry run must not leave a replayable terminal row"
    );
}

#[tokio::test]
async fn red_dry_run_warn_plans_without_audit_row() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let plan = svc
        .execute_dry_run(&warn_execution_keyed("req-dry-warn"))
        .await;
    assert_eq!(plan["dry_run"], json!(true), "got {plan}");
    assert!(discord.calls().is_empty());
    assert!(
        store.audits().is_empty(),
        "dry run must write zero audit rows, got {:?}",
        store.audits()
    );
}

#[tokio::test]
async fn control_baseline_dry_run_adapter_executes_and_returns_null() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let plan = <TestService as AdminDryRunExecute>::execute_dry_run(
        &svc,
        &ban_execution_keyed("req-baseline-dry"),
    )
    .await;
    assert_eq!(plan, Value::Null);
    assert_eq!(discord.calls().len(), 1);
    assert_eq!(store.audits().len(), 1);
}

// --- Confirm: executes once via claim/finish; replay refused ---

#[tokio::test]
async fn red_confirm_executes_once_and_replay_is_refused() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let confirm_id = "cnf_test_single_use_001";
    let first = svc
        .execute_confirmed(&ban_execution_keyed("req-confirm-1"), confirm_id)
        .await
        .expect("confirm executes");
    assert_eq!(first.outcome, MemberOutcome::Banned);
    assert!(!first.replayed);
    // Same confirm under a new key: refused, with zero further side effects.
    let replay = svc
        .execute_confirmed(&ban_execution_keyed("req-confirm-2"), confirm_id)
        .await;
    assert!(
        replay.is_err(),
        "confirm replay must be refused, got {replay:?}"
    );
    assert_eq!(
        discord.calls().len(),
        1,
        "confirm must execute exactly once, got {:?}",
        discord.calls()
    );
    assert_eq!(store.audits().len(), 1);
}

#[tokio::test]
async fn control_confirm_retry_under_same_key_replays_without_second_call() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let confirm_id = "cnf_test_retry_001";
    let exec = ban_execution_keyed("req-confirm-retry");
    let first = svc
        .execute_confirmed(&exec, confirm_id)
        .await
        .expect("confirm executes");
    assert!(!first.replayed);
    // Same key retried (e.g. after a client timeout): the stored outcome
    // replays through claim/finish instead of a confirm-reuse refusal.
    let second = svc
        .execute_confirmed(&exec, confirm_id)
        .await
        .expect("same-key retry replays");
    assert_eq!(second.outcome, MemberOutcome::Banned);
    assert!(second.replayed);
    assert_eq!(discord.calls().len(), 1);
    assert_eq!(store.audits().len(), 1);
}

#[tokio::test]
async fn control_baseline_confirm_adapter_ignores_confirm_id() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    for key in ["req-baseline-confirm-1", "req-baseline-confirm-2"] {
        let result = <TestService as AdminConfirmExecute>::execute_confirmed(
            &svc,
            &ban_execution_keyed(key),
            "cnf_baseline_ignored",
        )
        .await
        .expect("baseline adapter executes under the request key");
        assert_eq!(result.outcome, MemberOutcome::Banned);
        assert!(!result.replayed);
    }
    assert_eq!(discord.calls().len(), 2);
}

// --- Direct-with-audit controls: reversible verbs need no confirm ---

#[tokio::test]
async fn control_warn_executes_directly_with_audit_and_no_confirm() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let result = svc
        .execute(&warn_execution_keyed("req-warn-direct"))
        .await
        .expect("warn executes directly");
    assert_eq!(result.outcome, MemberOutcome::Warned);
    assert_eq!(store.audits().len(), 1);
    assert_eq!(
        store.audits()[0].action,
        ModerationAction::Warn.action_name()
    );
}

#[tokio::test]
async fn control_kick_executes_directly_with_audit_and_no_confirm() {
    let discord = MockMemberDiscord::new();
    let store = MemMemberStore::new();
    let svc = service_with(discord.clone(), store.clone());
    let result = svc
        .execute(&kick_execution())
        .await
        .expect("kick executes directly");
    assert_eq!(result.outcome, MemberOutcome::Kicked);
    assert_eq!(store.audits().len(), 1);
    match discord.calls().as_slice() {
        [DiscordCall::Kick { user_id, .. }] => assert_eq!(user_id, TARGET_ID),
        other => panic!("kick must issue one kick call, got {other:?}"),
    }
}
