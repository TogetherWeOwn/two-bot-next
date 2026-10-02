//! Reward effects use only the shared executor and the mock REST double.
#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::json;
use two_bot_core::leveling::{plan_reward_roles, LevelRoleReward, RewardRolePlan};
use two_bot_discord::{executor::StagingRevokeFence, ActionExecutor, DiscordError};

// Public pinned identities, used only to prove refusal and on the mock wire.
const GUILD: u64 = 1545644954272137297;
const PRODUCTION: u64 = 326474832151838730;
const USER: u64 = 3333;

fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("mock-only-token".into(), Some(mock.origin())).unwrap()
}

fn revoke_plan() -> RewardRolePlan {
    RewardRolePlan {
        grant: vec!["5555".into()],
        revoke: vec!["6666".into(), "7777".into()],
        grant_reason: "TWO leveling: reached level 1".into(),
        revoke_reason: Some("TWO leveling: below level thresholds at level 1".into()),
    }
}

fn preflight(
    manage_roles: bool,
    managed: bool,
    position: i64,
    missing: bool,
) -> Vec<ScriptedResponse> {
    let mut roles = vec![
        json!({"id":GUILD.to_string(),"permissions":"0","position":0,"managed":false}),
        json!({"id":"8888","permissions": if manage_roles {"268435456"} else {"0"},"position":10,"managed":false}),
        json!({"id":"6666","permissions":"0","position":1,"managed":false}),
    ];
    if !missing {
        roles.push(json!({"id":"7777","permissions":"0","position":position,"managed":managed}));
    }
    vec![
        ScriptedResponse::json(200, json!({"id":"4444"})),
        ScriptedResponse::json(200, json!({"roles":["8888"]})),
        ScriptedResponse::json(200, json!(roles)),
    ]
}

#[tokio::test]
async fn ordinary_grants_are_idempotent_and_never_remove_roles() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let executor = executor(&mock);
    let ladder = vec![LevelRoleReward {
        level: 1,
        role_id: "5555".into(),
    }];
    let plan = plan_reward_roles(1, &ladder, &["6666".into()], true, false);
    executor
        .execute_reward_roles(GUILD, USER, &plan, None)
        .await
        .unwrap();
    let again = plan_reward_roles(1, &ladder, &["5555".into(), "6666".into()], true, false);
    executor
        .execute_reward_roles(GUILD, USER, &again, None)
        .await
        .unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "PUT");
    assert_eq!(
        requests[0].path,
        format!("/api/v10/guilds/{GUILD}/members/3333/roles/5555")
    );
    assert!(requests[0]
        .header("x-audit-log-reason")
        .unwrap()
        .contains("reached"));
    mock.shutdown().await;
}

#[tokio::test]
async fn revoke_requires_explicit_staging_fence_before_any_io() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    let executor = executor(&mock);
    let fence = StagingRevokeFence::new(GUILD, PRODUCTION).unwrap();
    for (guild, fence) in [
        (GUILD, None),
        (PRODUCTION, Some(fence)),
        (1234, Some(fence)),
    ] {
        assert!(matches!(
            executor
                .execute_reward_roles(guild, USER, &revoke_plan(), fence)
                .await,
            Err(DiscordError::Rejected(_))
        ));
    }
    assert!(StagingRevokeFence::new(PRODUCTION, PRODUCTION).is_err());
    assert!(StagingRevokeFence::new(PRODUCTION, GUILD).is_err());
    assert!(StagingRevokeFence::new(1234, PRODUCTION).is_err());
    assert!(StagingRevokeFence::new(GUILD, 9999).is_err());
    assert_eq!(
        GUILD.to_string(),
        two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID
    );
    assert_eq!(
        PRODUCTION.to_string(),
        two_bot_core::backup::guild_config::LIVE_GUILD_ID
    );
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}

#[tokio::test]
async fn entire_revoke_set_is_refused_on_permission_hierarchy_managed_or_missing_roles() {
    for (permission, managed, position, missing) in [
        (false, false, 1, false),
        (true, true, 1, false),
        (true, false, 10, false),
        (true, false, 1, true),
    ] {
        let mock = MockRest::start(
            preflight(permission, managed, position, missing),
            ScriptedResponse::status(204),
        )
        .await;
        let result = executor(&mock)
            .execute_reward_roles(
                GUILD,
                USER,
                &revoke_plan(),
                Some(StagingRevokeFence::new(GUILD, PRODUCTION).unwrap()),
            )
            .await;
        assert!(matches!(result, Err(DiscordError::Rejected(_))));
        assert_eq!(mock.requests().len(), 3);
        assert!(mock.requests().iter().all(|r| r.method == "GET"));
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn authorized_staging_revoke_preflights_all_roles_and_preserves_unrelated_roles() {
    let mock = MockRest::start(
        preflight(true, false, 1, false),
        ScriptedResponse::status(204),
    )
    .await;
    executor(&mock)
        .execute_reward_roles(
            GUILD,
            USER,
            &revoke_plan(),
            Some(StagingRevokeFence::new(GUILD, PRODUCTION).unwrap()),
        )
        .await
        .unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 6);
    assert!(requests[..3].iter().all(|r| r.method == "GET"));
    assert_eq!(requests[3].method, "PUT");
    assert_eq!(requests[4].method, "DELETE");
    assert!(requests[4].path.ends_with("/roles/6666"));
    assert!(requests[5].path.ends_with("/roles/7777"));
    mock.shutdown().await;
}

#[tokio::test]
async fn forbidden_or_unreadable_role_readbacks_and_mutation_errors_are_observable() {
    for response in [
        ScriptedResponse::status(403),
        ScriptedResponse::json(200, json!({"not_roles":[]})),
    ] {
        let mock = MockRest::start(vec![response], ScriptedResponse::status(204)).await;
        assert!(executor(&mock).member_role_ids(GUILD, USER).await.is_err());
        assert_eq!(mock.requests().len(), 1);
        mock.shutdown().await;
    }
    let mock = MockRest::start(vec![], ScriptedResponse::status(403)).await;
    let mut plan = revoke_plan();
    plan.revoke.clear();
    assert!(matches!(
        executor(&mock)
            .execute_reward_roles(GUILD, USER, &plan, None)
            .await,
        Err(DiscordError::Rejected(_))
    ));
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}
