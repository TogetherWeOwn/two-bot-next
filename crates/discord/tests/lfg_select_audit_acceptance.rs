#![cfg(all(test, feature = "db"))]

//! Protected offline acceptance. Include the real interaction service unchanged;
//! only its external SQL/store/Discord boundaries are replaced by in-memory I/O.
//! No socket, environment credential, database or source-text assertion is used.

extern crate self as sqlx;
extern crate self as two_bot_core;
extern crate two_bot_core as real_core;

pub use real_core::{automation_quota, commands, lfg, rsvp};

#[path = "../src/automation_admission.rs"]
mod automation_admission;
#[path = "support/lfg_select_audit_offline.rs"]
mod offline;
#[path = "support/lfg_select_id_acceptance.rs"]
mod parser_acceptance;
pub use offline::postgres;
pub use offline::{
    lfg_store, query, query_as, query_scalar, rsvp_store, ActionExecutor, DiscordError, Error,
    PgPool,
};
#[path = "../src/lfg_interactions.rs"]
pub mod subject;

use lfg::{LfgSelectAction, LfgStatus};
use subject::{LfgError, LfgInteractions, LfgRequest};

fn signup(post_id: &str, role_key: &str) -> LfgRequest {
    LfgRequest::Select(LfgSelectAction::Signup {
        post_id: post_id.into(),
        role_key: role_key.into(),
    })
}

fn leave(post_id: &str) -> LfgRequest {
    LfgRequest::Select(LfgSelectAction::Leave {
        post_id: post_id.into(),
    })
}

async fn run(
    service: &LfgInteractions,
    executor: &ActionExecutor,
    request: LfgRequest,
    guild: &str,
    actor: &str,
    interaction: u64,
) -> Result<String, LfgError> {
    service
        .execute(executor, request, guild, actor, interaction, 99)
        .await
}

async fn invalid_id_refuses_before_io(post_id: &str, leaving: bool) {
    let pool = PgPool::fixture();
    // Seed even malformed IDs: validation must precede lookup, not just happen
    // to return missing when no row exists.
    pool.seed_post(post_id, "1");
    pool.seed_signup(post_id, "2", "tank");
    let service = LfgInteractions::new(pool.clone());
    let executor = ActionExecutor::default();
    let before = pool.snapshot();
    let request = if leaving {
        leave(post_id)
    } else {
        signup(post_id, "healer")
    };
    let result = run(&service, &executor, request, "1", "2", 100).await;
    assert!(
        pool.trace().is_empty() && executor.edits().is_empty(),
        "invalid ID reached lock/SQL/store/audit/Discord boundaries: {:?}",
        pool.trace()
    );
    assert_eq!(pool.snapshot(), before, "invalid ID changed durable state");
    assert!(
        matches!(result, Err(LfgError::Invalid(_))),
        "expected bounded input refusal, got {result:?}"
    );
    assert_eq!(
        result.expect_err("invalid ID").to_string(),
        "LFG id must be between 1 and 128 characters."
    );
}

macro_rules! invalid_id_case {
    ($name:ident, $id:expr, $leaving:expr) => {
        #[tokio::test]
        async fn $name() {
            invalid_id_refuses_before_io(&$id, $leaving).await;
        }
    };
}

invalid_id_case!(empty_signup_refuses_before_any_io, "", false);
invalid_id_case!(empty_leave_refuses_before_any_io, "", true);
invalid_id_case!(
    overlong_ascii_signup_refuses_before_any_io,
    "a".repeat(129),
    false
);
invalid_id_case!(
    overlong_ascii_leave_refuses_before_any_io,
    "a".repeat(129),
    true
);
invalid_id_case!(
    overlong_multibyte_signup_refuses_before_any_io,
    "é".repeat(129),
    false
);
invalid_id_case!(
    overlong_multibyte_leave_refuses_before_any_io,
    "é".repeat(129),
    true
);
invalid_id_case!(
    surrogate_overflow_signup_refuses_before_any_io,
    "😀".repeat(65),
    false
);
invalid_id_case!(
    surrogate_overflow_leave_refuses_before_any_io,
    "😀".repeat(65),
    true
);
invalid_id_case!(
    hostile_overlong_signup_refuses_before_any_io,
    format!("{}@everyone", "x".repeat(128)),
    false
);
invalid_id_case!(
    hostile_overlong_leave_refuses_before_any_io,
    format!("{}@everyone", "x".repeat(128)),
    true
);

#[tokio::test]
async fn valid_boundary_ids_still_apply_signup_and_leave() {
    for id in [
        "a".repeat(128),
        "é".repeat(100),
        "😀".repeat(64),
        " ".into(),
    ] {
        let pool = PgPool::fixture();
        pool.seed_post(&id, "1");
        let service = LfgInteractions::new(pool.clone());
        let executor = ActionExecutor::default();
        let result = run(&service, &executor, signup(&id, "tank"), "1", "2", 100).await;
        assert_eq!(result.expect("valid ID signup"), "LFG joined.");
        assert_eq!(pool.signup_role(&id, "2").as_deref(), Some("tank"));
        assert_eq!(
            run(&service, &executor, leave(&id), "1", "2", 101)
                .await
                .expect("valid ID leave"),
            "LFG left."
        );
        assert_eq!(pool.signup_role(&id, "2"), None);
        assert_eq!(pool.audits().len(), 2);
        assert_eq!(executor.edits().len(), 2);
    }
}

#[tokio::test]
async fn applied_signup_with_failed_audit_reports_honest_degradation() {
    let pool = PgPool::fixture();
    pool.seed_post("post-a", "1");
    pool.fail_audit(true);
    let service = LfgInteractions::new(pool.clone());
    let executor = ActionExecutor::default();
    let result = run(&service, &executor, signup("post-a", "tank"), "1", "2", 100).await;
    assert_eq!(
        pool.signup_role("post-a", "2").as_deref(),
        Some("tank"),
        "fault must occur after the real service applies the signup"
    );
    assert!(
        pool.audits().is_empty(),
        "injected failure must not manufacture an audit row"
    );
    assert_eq!(
        pool.audit_attempts(),
        1,
        "the service must actually encounter the audit fault"
    );
    let reply = result.expect("applied signup must return its outcome, not a bare audit error");
    let lower = reply.to_lowercase();
    assert!(lower.contains("joined"), "applied outcome missing: {reply}");
    assert!(
        lower.contains("audit")
            && ["not", "failed", "unavailable", "couldn't", "could not"]
                .iter()
                .any(|word| lower.contains(word)),
        "reply hides the lost audit: {reply}"
    );
    assert!(
        !reply.contains("offline audit fault"),
        "storage details must not be echoed"
    );
}

#[tokio::test]
async fn normal_signup_still_audits_and_refreshes_the_saved_state() {
    let pool = PgPool::fixture();
    pool.seed_post("post-a", "1");
    let service = LfgInteractions::new(pool.clone());
    let executor = ActionExecutor::default();
    assert_eq!(
        run(&service, &executor, signup("post-a", "tank"), "1", "2", 100)
            .await
            .expect("signup"),
        "LFG joined."
    );
    assert_eq!(pool.signup_role("post-a", "2").as_deref(), Some("tank"));
    let audits = pool.audits();
    assert_eq!(audits.len(), 1);
    assert_eq!(
        (
            &*audits[0].guild_id,
            audits[0].actor_id.as_deref(),
            &*audits[0].action,
            audits[0].target_key.as_deref(),
            &*audits[0].outcome
        ),
        ("1", Some("2"), "lfg.signup", Some("post-a"), "joined")
    );
    let edits = executor.edits();
    assert_eq!(edits.len(), 1);
    assert_eq!((&*edits[0].channel, &*edits[0].message), ("30", "40"));
    assert!(edits[0].content.contains("<@2>"));
}

async fn divergent_redelivery_refuses(request: LfgRequest, guild: &str, actor: &str) {
    let pool = PgPool::fixture();
    pool.seed_post("post-a", "1");
    pool.seed_post("post-b", "1");
    pool.seed_post("post-other-guild", "3");
    let executor = ActionExecutor::default();
    let service = LfgInteractions::new(pool.clone());
    assert_eq!(
        run(&service, &executor, signup("post-a", "tank"), "1", "2", 100)
            .await
            .expect("initial signup"),
        "LFG joined."
    );
    let before = pool.snapshot();
    let audit_attempts = pool.audit_attempts();
    let edits = executor.edits().len();
    // A fresh service excludes process-local cooldown/deduplication as proof of
    // durable identity binding. The stored interaction is the replay authority.
    let restarted = LfgInteractions::new(pool.clone());
    let result = run(&restarted, &executor, request, guild, actor, 100).await;
    assert!(
        result.is_err(),
        "changed request reused a prior interaction instead of being refused: {result:?}"
    );
    assert_eq!(
        pool.snapshot(),
        before,
        "divergent redelivery changed state or audit"
    );
    assert_eq!(
        pool.audit_attempts(),
        audit_attempts,
        "divergence must not attempt another audit append"
    );
    assert_eq!(
        executor.edits().len(),
        edits,
        "divergence must not refresh a channel"
    );
}

#[tokio::test]
async fn changed_role_is_not_an_identical_redelivery() {
    divergent_redelivery_refuses(signup("post-a", "healer"), "1", "2").await;
}

#[tokio::test]
async fn changed_actor_is_not_an_identical_redelivery() {
    divergent_redelivery_refuses(signup("post-a", "tank"), "1", "4").await;
}

#[tokio::test]
async fn changed_post_is_not_an_identical_redelivery() {
    divergent_redelivery_refuses(signup("post-b", "tank"), "1", "2").await;
}

#[tokio::test]
async fn changed_guild_is_not_an_identical_redelivery() {
    divergent_redelivery_refuses(signup("post-other-guild", "tank"), "3", "2").await;
}

#[tokio::test]
async fn leave_cannot_reuse_a_signup_interaction_identity() {
    divergent_redelivery_refuses(leave("post-a"), "1", "2").await;
}

#[tokio::test]
async fn identical_redelivery_after_restart_has_one_audit_and_no_second_signup() {
    let pool = PgPool::fixture();
    pool.seed_post("post-a", "1");
    let executor = ActionExecutor::default();
    let service = LfgInteractions::new(pool.clone());
    assert_eq!(
        run(&service, &executor, signup("post-a", "tank"), "1", "2", 100)
            .await
            .expect("initial signup"),
        "LFG joined."
    );
    let before = pool.snapshot();
    let attempts = pool.audit_attempts();
    let mutations = pool.signup_calls();
    let restarted = LfgInteractions::new(pool.clone());
    assert_eq!(
        run(
            &restarted,
            &executor,
            signup("post-a", "tank"),
            "1",
            "2",
            100
        )
        .await
        .expect("identical replay"),
        "LFG joined."
    );
    assert_eq!(pool.snapshot(), before);
    assert_eq!(pool.audit_attempts(), attempts);
    assert_eq!(
        pool.signup_calls(),
        mutations,
        "replay must not re-adjudicate an identical signup"
    );
    assert_eq!(
        executor.edits().len(),
        2,
        "identical replay may recover the existing message refresh"
    );
}

#[tokio::test]
async fn identical_redelivery_after_leave_does_not_resurrect_signup() {
    let pool = PgPool::fixture();
    pool.seed_post("post-a", "1");
    let executor = ActionExecutor::default();
    let service = LfgInteractions::new(pool.clone());
    run(&service, &executor, signup("post-a", "tank"), "1", "2", 100)
        .await
        .expect("signup");
    run(&service, &executor, leave("post-a"), "1", "2", 101)
        .await
        .expect("leave");
    let before = pool.snapshot();
    let restarted = LfgInteractions::new(pool.clone());
    assert_eq!(
        run(
            &restarted,
            &executor,
            signup("post-a", "tank"),
            "1",
            "2",
            100
        )
        .await
        .expect("replay"),
        "LFG joined."
    );
    assert_eq!(pool.signup_role("post-a", "2"), None);
    assert_eq!(pool.snapshot(), before);
    assert_eq!(
        pool.audits()
            .iter()
            .filter(|row| row.action == "lfg.signup")
            .count(),
        1
    );
}

#[tokio::test]
async fn refresh_failure_keeps_the_existing_honest_saved_state_suffix() {
    let pool = PgPool::fixture();
    pool.seed_post("post-a", "1");
    let executor = ActionExecutor::default();
    executor.fail_refresh(true);
    let service = LfgInteractions::new(pool.clone());
    let reply = run(&service, &executor, signup("post-a", "tank"), "1", "2", 100)
        .await
        .expect("saved signup");
    assert_eq!(
        reply,
        "LFG joined. Message refresh failed; the saved result above still applies."
    );
    assert_eq!(pool.signup_role("post-a", "2").as_deref(), Some("tank"));
    assert_eq!(pool.audits().len(), 1);
    assert_eq!(pool.snapshot().posts[0].status, LfgStatus::Open);
}
