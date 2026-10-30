//! Protected acceptance: partial role-grant application reports what happened.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the acceptance branch, keep
//! every test here verbatim, and make the RED tests pass by changing
//! production code only. Do not edit the fixtures or assertions: they are the
//! spec. The existing `leveling_roles.rs` tests are NOT protected; update them
//! for the new signature.
//!
//! Baseline behavior under test: `execute_reward_roles` issues one paced REST
//! call per grant and per revoke and aborts with `?` on the first refusal, so
//! a mid-list 403 leaves earlier roles applied and later roles skipped with no
//! structured record of which is which: the caller sees an opaque `Err`.
//!
//! Required behavior:
//! - `execute_reward_roles` keeps its parameters and error type but returns
//!   `Result<(applied, failed, skipped), DiscordError>` where `applied` lists
//!   every role id written (grants first, then revokes, in plan order),
//!   `failed` is the first refused role with its reason, and `skipped` lists
//!   every role never attempted. A `type` alias over this exact tuple is
//!   welcome, but the underlying type must stay this tuple: a named struct
//!   will not match this file's adapter.
//! - A mid-application refusal returns `Ok` with the populated report instead
//!   of an early `Err`. Fail-fast is preserved: the first refusal aborts all
//!   remaining writes, so a grant failure also skips pending revokes.
//! - Pre-mutation refusals (revoke fence, missing reason, revoke preflight,
//!   malformed ids) stay `Err(DiscordError::Rejected(..))` with no report.
//! - The failed reason preserves the refusal evidence: it contains `403` or
//!   `forbidden` (case-insensitive) for a Discord 403.
//! - Audit reasons are preserved: every attempted write carries the plan's
//!   grant or revoke reason on the wire.
//!
//! The [`ExecutionReport`] adapter accepts both the legacy `Result<(), _>`
//! and the tuple shape, so this file compiles on the baseline and fails on
//! behavior (mid-list refusal yields `Err`, never a report), not on a
//! missing API.
//!
//! Offline only: the mock REST double. No database, no live Discord.

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::json;
use two_bot_core::leveling::{grant_audit_reason, revoke_audit_reason, RewardRolePlan};
use two_bot_discord::{executor::StagingRevokeFence, ActionExecutor, DiscordError};

// Public pinned identities, used only on the mock wire.
const GUILD: u64 = 1545644954272137297;
const PRODUCTION: u64 = 326474832151838730;
const USER: u64 = 3333;

/// The exact report tuple `execute_reward_roles` must return. Positions:
/// `.0` applied role ids, `.1` the first failed role plus its reason,
/// `.2` role ids never attempted.
type ReportTuple = (Vec<String>, Option<(String, String)>, Vec<String>);

#[derive(Debug, PartialEq, Eq)]
struct ReportShape {
    applied: Vec<String>,
    failed: Option<(String, String)>,
    skipped: Vec<String>,
}

/// Adapter over the executor's return shape: the legacy unit result maps
/// `Ok(())` to a fully-applied report (every planned write landed) and
/// propagates `Err`; the tuple shape maps field for field.
trait ExecutionReport {
    fn acceptance_report(self, plan: &RewardRolePlan) -> Result<ReportShape, DiscordError>;
}

impl ExecutionReport for Result<(), DiscordError> {
    fn acceptance_report(self, plan: &RewardRolePlan) -> Result<ReportShape, DiscordError> {
        self.map(|()| ReportShape {
            applied: plan
                .grant
                .iter()
                .chain(plan.revoke.iter())
                .cloned()
                .collect(),
            failed: None,
            skipped: Vec::new(),
        })
    }
}

impl ExecutionReport for Result<ReportTuple, DiscordError> {
    fn acceptance_report(self, _plan: &RewardRolePlan) -> Result<ReportShape, DiscordError> {
        self.map(|(applied, failed, skipped)| ReportShape {
            applied,
            failed,
            skipped,
        })
    }
}

fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("mock-only-token".into(), Some(mock.origin())).unwrap()
}

fn grant_plan(ids: &[&str], level: u64) -> RewardRolePlan {
    RewardRolePlan {
        grant: ids.iter().map(ToString::to_string).collect(),
        revoke: Vec::new(),
        grant_reason: grant_audit_reason(level),
        revoke_reason: None,
    }
}

fn revoke_plan(ids: &[&str], level: u64) -> RewardRolePlan {
    RewardRolePlan {
        grant: Vec::new(),
        revoke: ids.iter().map(ToString::to_string).collect(),
        grant_reason: grant_audit_reason(level),
        revoke_reason: Some(revoke_audit_reason(level)),
    }
}

/// Revoke preflight script for an arbitrary target set: the bot holds one
/// `MANAGE_ROLES` role above every target, and every target is present,
/// unmanaged and below the bot.
fn preflight_for(targets: &[&str]) -> Vec<ScriptedResponse> {
    let mut roles = vec![
        json!({"id":GUILD.to_string(),"permissions":"0","position":0,"managed":false}),
        json!({"id":"8888","permissions":"268435456","position":10,"managed":false}),
    ];
    for id in targets {
        roles.push(json!({"id":id,"permissions":"0","position":1,"managed":false}));
    }
    vec![
        ScriptedResponse::json(200, json!({"id":"4444"})),
        ScriptedResponse::json(200, json!({"roles":["8888"]})),
        ScriptedResponse::json(200, json!(roles)),
    ]
}

fn fence() -> StagingRevokeFence {
    StagingRevokeFence::new(GUILD, PRODUCTION).unwrap()
}

fn audit_reason(request: &common::RestRequest) -> String {
    decode_reason(request.header("x-audit-log-reason").unwrap_or_default())
}

// The client percent-encodes the audit-log reason on the wire, so decode
// before asserting on its text (same helper as the moderation suites).
fn decode_reason(header: &str) -> String {
    let mut bytes = Vec::new();
    let input = header.as_bytes();
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' {
            bytes.push(u8::from_str_radix(&header[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            bytes.push(input[i]);
            i += 1;
        }
    }
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn mid_list_grant_refusal_reports_applied_failed_and_skipped() {
    let plan = grant_plan(&["5001", "5002", "5003"], 5);
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204), ScriptedResponse::status(403)],
        ScriptedResponse::status(500),
    )
    .await;
    let report = executor(&mock)
        .execute_reward_roles(GUILD, USER, &plan, None)
        .await
        .acceptance_report(&plan)
        .expect("a mid-list refusal must return a structured report, not an opaque error");
    assert_eq!(report.applied, vec!["5001".to_owned()]);
    let (failed_id, reason) = report.failed.expect("the refused role is reported");
    assert_eq!(failed_id, "5002");
    assert!(
        reason.contains("403") || reason.to_ascii_lowercase().contains("forbidden"),
        "the failed reason preserves the refusal evidence: {reason}"
    );
    assert_eq!(report.skipped, vec!["5003".to_owned()]);

    // Fail-fast on the wire: two PUTs, then nothing. Both attempts carry the
    // grant audit reason.
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "PUT");
    assert!(requests[0].path.ends_with("/roles/5001"));
    assert_eq!(requests[1].method, "PUT");
    assert!(requests[1].path.ends_with("/roles/5002"));
    for request in &requests {
        assert!(
            audit_reason(request).contains("reached level 5"),
            "attempted writes keep the grant audit reason: {}",
            audit_reason(request)
        );
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn mid_list_revoke_refusal_reports_applied_failed_and_skipped() {
    let plan = revoke_plan(&["6001", "6002", "6003"], 5);
    let mut script = preflight_for(&["6001", "6002", "6003"]);
    script.push(ScriptedResponse::status(204));
    script.push(ScriptedResponse::status(403));
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let report = executor(&mock)
        .execute_reward_roles(GUILD, USER, &plan, Some(fence()))
        .await
        .acceptance_report(&plan)
        .expect("a mid-list revoke refusal must return a structured report");
    assert_eq!(report.applied, vec!["6001".to_owned()]);
    let (failed_id, reason) = report.failed.expect("the refused revoke is reported");
    assert_eq!(failed_id, "6002");
    assert!(
        reason.contains("403") || reason.to_ascii_lowercase().contains("forbidden"),
        "the failed reason preserves the refusal evidence: {reason}"
    );
    assert_eq!(report.skipped, vec!["6003".to_owned()]);

    // Three preflight GETs, one applied DELETE, one refused DELETE, then stop.
    // Both DELETEs carry the revoke audit reason.
    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    assert!(requests[..3].iter().all(|r| r.method == "GET"));
    assert_eq!(requests[3].method, "DELETE");
    assert!(requests[3].path.ends_with("/roles/6001"));
    assert_eq!(requests[4].method, "DELETE");
    assert!(requests[4].path.ends_with("/roles/6002"));
    for request in &requests[3..] {
        assert!(
            audit_reason(request).contains("below level thresholds at level 5"),
            "attempted revokes keep the revoke audit reason: {}",
            audit_reason(request)
        );
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn grant_failure_skips_pending_revokes() {
    let plan = RewardRolePlan {
        grant: vec!["5001".to_owned(), "5002".to_owned()],
        revoke: vec!["6001".to_owned()],
        grant_reason: grant_audit_reason(5),
        revoke_reason: Some(revoke_audit_reason(5)),
    };
    let mut script = preflight_for(&["6001"]);
    script.push(ScriptedResponse::status(204));
    script.push(ScriptedResponse::status(403));
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let report = executor(&mock)
        .execute_reward_roles(GUILD, USER, &plan, Some(fence()))
        .await
        .acceptance_report(&plan)
        .expect("a grant failure must still return a structured report");
    assert_eq!(report.applied, vec!["5001".to_owned()]);
    assert_eq!(
        report.failed.as_ref().map(|(id, _)| id.as_str()),
        Some("5002")
    );
    assert_eq!(report.skipped, vec!["6001".to_owned()]);

    // The pending revoke is never attempted: no DELETE on the wire.
    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    assert!(requests.iter().all(|r| r.method != "DELETE"));
    mock.shutdown().await;
}

#[tokio::test]
async fn all_success_reports_full_application() {
    let plan = grant_plan(&["5001", "5002"], 5);
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let report = executor(&mock)
        .execute_reward_roles(GUILD, USER, &plan, None)
        .await
        .acceptance_report(&plan)
        .expect("a clean run reports full application");
    assert_eq!(report.applied, vec!["5001".to_owned(), "5002".to_owned()]);
    assert!(report.failed.is_none());
    assert!(report.skipped.is_empty());
    assert_eq!(mock.requests().len(), 2);
    mock.shutdown().await;
}

#[tokio::test]
async fn empty_plan_applies_nothing() {
    let plan = grant_plan(&[], 5);
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let report = executor(&mock)
        .execute_reward_roles(GUILD, USER, &plan, None)
        .await
        .acceptance_report(&plan)
        .expect("an empty plan reports empty application");
    assert!(report.applied.is_empty());
    assert!(report.failed.is_none());
    assert!(report.skipped.is_empty());
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn preflight_refusal_stays_an_error_without_a_report() {
    // Pre-mutation refusals never start applying, so there is no partial
    // outcome to report: they stay `Err`, exactly as today.
    let plan = revoke_plan(&["6001", "6002"], 5);
    let roles = vec![
        json!({"id":GUILD.to_string(),"permissions":"0","position":0,"managed":false}),
        json!({"id":"8888","permissions":"0","position":10,"managed":false}),
        json!({"id":"6001","permissions":"0","position":1,"managed":false}),
        json!({"id":"6002","permissions":"0","position":1,"managed":false}),
    ];
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id":"4444"})),
            ScriptedResponse::json(200, json!({"roles":["8888"]})),
            ScriptedResponse::json(200, json!(roles)),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let result = executor(&mock)
        .execute_reward_roles(GUILD, USER, &plan, Some(fence()))
        .await
        .acceptance_report(&plan);
    assert!(
        matches!(result, Err(DiscordError::Rejected(_))),
        "a preflight refusal without MANAGE_ROLES stays an error"
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|r| r.method == "GET"));
    mock.shutdown().await;
}
