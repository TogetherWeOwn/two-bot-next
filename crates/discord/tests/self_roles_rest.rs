#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use two_bot_core::self_roles::{PanelMode, SelfRoleOption, SelfRolePanel};
use two_bot_discord::{executor::self_roles::SelfRoleRestError, ActionExecutor};

const GUILD: &str = "100000000000000001";
const USER: &str = "100000000000000002";
const BOT: &str = "100000000000000003";
const ROLE: &str = "100000000000000004";
const BOT_ROLE: &str = "100000000000000005";
const OTHER: &str = "100000000000000006";
const CHANNEL: &str = "100000000000000007";
const MESSAGE: &str = "100000000000000008";

fn executor(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("fixture-bot-token".into(), Some(mock.origin())).unwrap()
}
fn panel() -> SelfRolePanel {
    SelfRolePanel {
        id: "games".into(),
        channel_id: CHANNEL.into(),
        message_id: MESSAGE.into(),
        mode: PanelMode::Button,
        exclusive: false,
        color: false,
        options: vec![SelfRoleOption {
            key: "chess".into(),
            label: "Chess".into(),
            role_id: ROLE.into(),
            permissions: "0".into(),
            emoji: None,
            description: None,
        }],
    }
}
fn script(position: i64, permissions: &str, overwrite: serde_json::Value) -> Vec<ScriptedResponse> {
    vec![
        ScriptedResponse::json(200, json!({"user":{"id":USER},"roles":[OTHER]})),
        ScriptedResponse::json(
            200,
            json!({"user":{"id":BOT,"bot":true},"roles":[BOT_ROLE]}),
        ),
        ScriptedResponse::json(
            200,
            json!([
                {"id":GUILD,"permissions":"1024","position":0,"managed":false,"color":0},
                {"id":ROLE,"permissions":permissions,"position":position,"managed":false,"color":0},
                {"id":BOT_ROLE,"permissions":"268435456","position":10,"managed":true,"color":0},
                {"id":OTHER,"permissions":"0","position":1,"managed":false,"color":0}
            ]),
        ),
        ScriptedResponse::json(
            200,
            json!([
                {"id":CHANNEL,"guild_id":GUILD,"name":"games","permission_overwrites":overwrite}
            ]),
        ),
    ]
}

#[tokio::test]
async fn fresh_member_and_complete_live_policy_use_shared_paced_reads() {
    let mock = MockRest::start(script(2, "0", json!([])), ScriptedResponse::status(500)).await;
    let snapshot = executor(&mock)
        .fetch_self_role_snapshot(GUILD, USER, BOT)
        .await
        .unwrap();
    assert!(snapshot.member_role_ids.contains(OTHER));
    assert!(!snapshot.member_role_ids.contains(ROLE));
    assert!(!snapshot.member_is_bot);
    assert!(snapshot.validate(GUILD, &panel(), &[ROLE.into()]).is_none());
    let calls = mock.requests();
    assert_eq!(
        calls.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
        vec![
            format!("/api/v10/guilds/{GUILD}/members/{USER}"),
            format!("/api/v10/guilds/{GUILD}/members/{BOT}"),
            format!("/api/v10/guilds/{GUILD}/roles"),
            format!("/api/v10/guilds/{GUILD}/channels"),
        ]
    );
    for adjacent in calls.windows(2) {
        assert!(
            adjacent[1]
                .received_at
                .duration_since(adjacent[0].received_at)
                >= Duration::from_millis(100)
        );
    }
    assert!(calls.iter().all(|c| c.method == "GET"));
    mock.shutdown().await;
}

#[tokio::test]
async fn live_hierarchy_permission_drift_and_channel_grants_are_refused() {
    for (position, permissions, overwrites, code) in [
        (10, "0", json!([]), "role_hierarchy"),
        (2, "1024", json!([]), "role_permissions_changed"),
        (
            2,
            "0",
            json!([{"id":ROLE,"type":0,"allow":"8","deny":"0"}]),
            "disallowed_channel_permission",
        ),
    ] {
        let mock = MockRest::start(
            script(position, permissions, overwrites),
            ScriptedResponse::status(204),
        )
        .await;
        let snapshot = executor(&mock)
            .fetch_self_role_snapshot(GUILD, USER, BOT)
            .await
            .unwrap();
        assert_eq!(
            snapshot
                .validate(GUILD, &panel(), &[ROLE.into()])
                .unwrap()
                .code,
            code
        );
        assert!(mock.requests().iter().all(|c| c.method == "GET"));
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn malformed_duplicate_or_missing_policy_fails_closed() {
    for kind in [
        "missing_everyone",
        "duplicate",
        "unknown_member_role",
        "bad_mask",
        "wrong_guild",
        "missing_overwrites",
        "wrong_member",
    ] {
        let mut responses = script(2, "0", json!([]));
        let mut roles: serde_json::Value = serde_json::from_slice(&responses[2].body).unwrap();
        match kind {
            "missing_everyone" => {
                roles.as_array_mut().unwrap().remove(0);
            }
            "duplicate" => {
                let dupe = roles[0].clone();
                roles.as_array_mut().unwrap().push(dupe);
            }
            "unknown_member_role" => {
                roles.as_array_mut().unwrap().pop();
            }
            "bad_mask" => {
                roles[1]["permissions"] = json!("-1");
            }
            "wrong_guild" => {
                responses[3] = ScriptedResponse::json(
                    200,
                    json!([{ "id":CHANNEL, "guild_id":OTHER, "permission_overwrites":[]} ]),
                );
            }
            "missing_overwrites" => {
                responses[3] =
                    ScriptedResponse::json(200, json!([{ "id":CHANNEL, "guild_id":GUILD } ]));
            }
            "wrong_member" => {
                responses[0] = ScriptedResponse::json(200, json!({"user":{"id":OTHER},"roles":[]}));
            }
            _ => unreachable!(),
        }
        responses[2] = ScriptedResponse::json(200, roles);
        let mock = MockRest::start(responses, ScriptedResponse::status(204)).await;
        assert_eq!(
            executor(&mock)
                .fetch_self_role_snapshot(GUILD, USER, BOT)
                .await
                .unwrap_err(),
            SelfRoleRestError::Snapshot,
            "{kind}"
        );
        assert!(mock.requests().iter().all(|c| c.method == "GET"));
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn reaction_message_partials_are_fetched_and_identity_is_verified() {
    for id in [MESSAGE, OTHER] {
        let mock = MockRest::start(
            vec![ScriptedResponse::json(
                200,
                json!({"id":id,"channel_id":CHANNEL}),
            )],
            ScriptedResponse::status(500),
        )
        .await;
        let result = executor(&mock)
            .fetch_self_role_message(CHANNEL, MESSAGE)
            .await;
        assert_eq!(
            result,
            if id == MESSAGE {
                Ok(())
            } else {
                Err(SelfRoleRestError::Snapshot)
            }
        );
        assert_eq!(
            mock.requests()[0].path,
            format!("/api/v10/channels/{CHANNEL}/messages/{MESSAGE}")
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn singular_add_and_remove_never_replace_unrelated_member_roles() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204), ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(&mock);
    for add in [false, true] {
        let exchange = exec
            .self_role_step(GUILD, USER, ROLE, add, || async { Ok(true) })
            .await
            .unwrap();
        assert_eq!(exchange.result, Ok(()));
        assert!(exchange.owned_after);
    }
    let calls = mock.requests();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].method, "DELETE");
    assert_eq!(calls[1].method, "PUT");
    assert!(calls.iter().all(|c| c.path
        == format!("/api/v10/guilds/{GUILD}/members/{USER}/roles/{ROLE}")
        && c.body.is_empty()));
    mock.shutdown().await;
}

#[tokio::test]
async fn rejected_rate_limited_and_uncertain_exchanges_do_not_retry_or_leak_bodies() {
    for (status, error) in [
        (403, SelfRoleRestError::Rejected(403)),
        (429, SelfRoleRestError::RateLimited),
        (503, SelfRoleRestError::Ambiguous),
        (302, SelfRoleRestError::Ambiguous),
        (200, SelfRoleRestError::Ambiguous),
    ] {
        let mock = MockRest::start(
            vec![ScriptedResponse::json(
                status,
                json!({"message":"provider-secret"}),
            )],
            ScriptedResponse::status(204),
        )
        .await;
        let exchange = executor(&mock)
            .self_role_step(GUILD, USER, ROLE, true, || async { Ok(true) })
            .await
            .unwrap();
        assert_eq!(exchange.result, Err(error));
        assert!(exchange.owned_after);
        assert!(exchange.response_received);
        assert!(!format!("{exchange:?}").contains("provider-secret"));
        assert_eq!(mock.requests().len(), 1);
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn lost_post_call_ownership_retains_accepted_effect() {
    let checks = AtomicUsize::new(0);
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    let exchange = executor(&mock)
        .self_role_step(GUILD, USER, ROLE, true, || async {
            Ok(checks.fetch_add(1, Ordering::SeqCst) < 2)
        })
        .await
        .unwrap();
    assert_eq!(exchange.result, Ok(()));
    assert!(!exchange.owned_after);
    assert!(exchange.response_received);
    assert_eq!(checks.load(Ordering::SeqCst), 3);
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn timeout_cannot_claim_remote_completion_or_retry() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204).delayed(Duration::from_secs(6))],
        ScriptedResponse::status(204),
    )
    .await;
    let exchange = executor(&mock)
        .self_role_step(GUILD, USER, ROLE, true, || async { Ok(true) })
        .await
        .unwrap();
    assert_eq!(exchange.result, Err(SelfRoleRestError::Ambiguous));
    assert!(exchange.owned_after);
    assert!(!exchange.response_received);
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn late_fence_is_evaluated_after_shared_pacing_wait() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    let exec = executor(&mock);
    exec.self_role_step(GUILD, USER, ROLE, true, || async { Ok(true) })
        .await
        .unwrap();
    let fence_calls = Arc::clone(&calls);
    let previous_send = mock.requests()[0].received_at;
    let result = exec
        .self_role_step(GUILD, USER, ROLE, false, || async {
            assert!(previous_send.elapsed() >= Duration::from_millis(100));
            fence_calls.fetch_add(1, Ordering::SeqCst);
            Ok(false)
        })
        .await;
    assert_eq!(result.unwrap_err(), SelfRoleRestError::StaleClaim);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}

#[tokio::test]
async fn failed_journal_or_ownership_lost_during_journal_prevents_send() {
    for failed_journal in [true, false] {
        let checks = AtomicUsize::new(0);
        let journals = AtomicUsize::new(0);
        let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
        let result = executor(&mock)
            .self_role_step_journaled(
                GUILD,
                USER,
                ROLE,
                true,
                || async { Ok(checks.fetch_add(1, Ordering::SeqCst) == 0) },
                || async {
                    journals.fetch_add(1, Ordering::SeqCst);
                    if failed_journal {
                        Err(SelfRoleRestError::StaleClaim)
                    } else {
                        Ok(())
                    }
                },
            )
            .await;
        assert_eq!(result.unwrap_err(), SelfRoleRestError::StaleClaim);
        assert_eq!(journals.load(Ordering::SeqCst), 1);
        assert_eq!(
            checks.load(Ordering::SeqCst),
            if failed_journal { 1 } else { 2 }
        );
        assert!(mock.requests().is_empty());
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn everyone_or_malformed_ids_never_reach_the_wire() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    for role in [GUILD, "0", "100000000000000004/other"] {
        assert_eq!(
            executor(&mock)
                .self_role_step(GUILD, USER, role, true, || async { Ok(true) })
                .await
                .unwrap_err(),
            SelfRoleRestError::InvalidId
        );
    }
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}
