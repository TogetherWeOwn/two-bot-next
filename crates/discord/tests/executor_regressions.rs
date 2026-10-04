//! S4 regression tests ([TOG-10076](/TOG/issues/TOG-10076)): pins for the eight
//! findings from the [TOG-10718](/TOG/issues/TOG-10718) review of PR #63.
//!
//! Each test drives the executor against the scripted REST side of the mock
//! Discord double and asserts on the wire (recorded requests), never on
//! client-side timers.
//!
//! Dev-only: never ships in the release binary.

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use two_bot_core::{ActionOutcome, ModerationAction, ModerationExecution};
use two_bot_discord::{ActionExecutor, ChannelCall, DiscordError};

const GUILD: &str = "2222";
const USER: &str = "3333";
const CHANNEL: &str = "4444";
const REASON: &str = "s4 regression";

fn executor_for(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("s4-regression-token".to_owned(), Some(mock.origin()))
        .expect("executor builds against the mock")
}

fn context(action: ModerationAction, count: Option<u64>) -> ModerationExecution {
    ModerationExecution {
        guild_id: GUILD.to_owned(),
        channel_id: Some(CHANNEL.to_owned()),
        action,
        target_user_id: Some(USER.to_owned()),
        reason: REASON.to_owned(),
        duration_seconds: None,
        count,
        seconds: None,
    }
}

#[tokio::test]
async fn original_response_edit_suppresses_mentions_bounds_content_and_validates_ids() {
    // The `@original` PATCH requires a validated message id receipt under
    // main's durable send admission; a bare 200 is `Unavailable`, not success.
    let receipt = || ScriptedResponse::json(200, serde_json::json!({"id": "555555555555555555"}));
    let mock = MockRest::start(vec![receipt(), receipt()], ScriptedResponse::status(500)).await;
    let executor = executor_for(&mock);
    executor
        .edit_interaction_response(1234, "synthetic-webhook-token", "@everyone <@3333> result")
        .await
        .unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "PATCH");
    assert_eq!(
        requests[0].path,
        "/api/v10/webhooks/1234/synthetic-webhook-token/messages/@original"
    );
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["allowed_mentions"]["parse"], serde_json::json!([]));
    // The executor rewrites mentions explicitly rather than relying on
    // Twilight's omitted-empty serialization: empty allowlists are present
    // but ping nobody, and parse=[] disables all pings.
    assert_eq!(body["allowed_mentions"]["users"], serde_json::json!([]));
    assert_eq!(body["allowed_mentions"]["roles"], serde_json::json!([]));
    assert!(matches!(
        executor
            .edit_interaction_response(0, "synthetic-webhook-token", "result")
            .await,
        Err(DiscordError::Rejected(_))
    ));
    assert_eq!(
        mock.requests().len(),
        1,
        "invalid id never reaches the wire"
    );
    executor
        .edit_interaction_response(1234, "synthetic-webhook-token", &"x".repeat(2001))
        .await
        .unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let bounded: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(
        bounded["content"].as_str().unwrap().encode_utf16().count(),
        2000
    );
    mock.shutdown().await;
}

// Finding 1: a normal 600 s timeout must reach Discord with a timestamp the
// pinned Twilight parser accepts (the `Z` form never survived `Timestamp`).
#[tokio::test]
async fn timeout_outcome_sends_a_valid_timestamp() {
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(200, serde_json::json!({"user": {"id": USER}})),
    )
    .await;
    let result = executor_for(&mock)
        .execute_outcome(
            &context(ModerationAction::Timeout, None),
            &ActionOutcome::TimedOut {
                user_id: USER.to_owned(),
                duration_seconds: 600,
            },
        )
        .await;
    assert!(
        result.is_ok(),
        "normal 600 s timeout produced {result:?}; {} wire requests",
        mock.requests().len()
    );
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1, "exactly one wire call");
    assert_eq!(reqs[0].method, "PATCH");
    let body: serde_json::Value =
        serde_json::from_slice(&reqs[0].body).expect("timeout body is JSON");
    let until = body["communication_disabled_until"]
        .as_str()
        .expect("timeout PATCH carries communication_disabled_until");
    assert!(
        until.ends_with("+00:00"),
        "until uses the parser-accepted offset form, got {until}"
    );
    mock.shutdown().await;
}

// Finding 7: a 29-day timeout is rejected by Twilight before any send — it
// must map to Rejected (safe pre-mutation), never Unavailable.
#[tokio::test]
async fn local_timeout_validation_is_safe_pre_mutation() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(200)).await;
    let until = two_bot_discord::timeout_until_iso(29 * 86400);
    let err = executor_for(&mock)
        .timeout_member(GUILD, USER, Some(&until), REASON)
        .await
        .expect_err("29-day timeout must be rejected locally");
    assert!(
        matches!(err, DiscordError::Rejected(_)),
        "pre-send validation maps to Rejected, got {err:?}"
    );
    assert!(err.is_safe_pre_mutation());
    assert!(mock.requests().is_empty(), "zero wire requests");
    mock.shutdown().await;
}

// Finding 2: guild-members reads keep their route and query.
#[tokio::test]
async fn guild_members_get_preserves_route_and_query() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, serde_json::json!([]))).await;
    executor_for(&mock)
        .get_json("/guilds/2222/members?limit=1000")
        .await
        .expect("members read succeeds");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].path, "/api/v10/guilds/2222/members?limit=1000",
        "route and limit preserved, nothing substituted"
    );
    mock.shutdown().await;
}

// Finding 2: history cursors survive — dropping `before` would re-read the
// newest page forever.
#[tokio::test]
async fn history_get_preserves_before_cursor() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, serde_json::json!([]))).await;
    executor_for(&mock)
        .get_json("/channels/4444/messages?limit=100&before=9999")
        .await
        .expect("history read succeeds");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert!(
        reqs[0].path.contains("before=9999"),
        "before cursor preserved, observed {}",
        reqs[0].path
    );
    assert!(
        reqs[0].path.contains("limit=100"),
        "limit preserved, observed {}",
        reqs[0].path
    );
    mock.shutdown().await;
}

// Finding 2: scheduled-events reads render with their flag.
#[tokio::test]
async fn scheduled_events_get_preserves_user_count_flag() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, serde_json::json!([]))).await;
    executor_for(&mock)
        .get_json("/guilds/2222/scheduled-events?with_user_count=true")
        .await
        .expect("scheduled-events read succeeds");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert!(
        reqs[0]
            .path
            .contains("/api/v10/guilds/2222/scheduled-events"),
        "observed {}",
        reqs[0].path
    );
    assert!(
        reqs[0].path.contains("with_user_count=true"),
        "flag preserved, observed {}",
        reqs[0].path
    );
    mock.shutdown().await;
}

// Finding 2: unsupported paths are refused without I/O — never silently
// rewritten to another resource.
#[tokio::test]
async fn unsupported_get_path_is_refused_without_io() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(200)).await;
    let exec = executor_for(&mock);
    for path in [
        "/guilds/2222/bogus",
        "/channels/4444/bogus",
        "/users/3333",
        "/channels/not-a-snowflake",
    ] {
        assert!(
            exec.get_json(path).await.is_err(),
            "unsupported path refused: {path}"
        );
    }
    assert!(mock.requests().is_empty(), "zero wire requests");
    mock.shutdown().await;
}

// Finding 3: HTTP 200 with invalid JSON is unreadable — refuse with the one
// read on the wire, never PUT a fabricated zero-mask overwrite.
#[tokio::test]
async fn unreadable_channel_does_not_mutate_permissions() {
    let mock = MockRest::start(
        vec![ScriptedResponse {
            body: b"invalid-json".to_vec(),
            ..ScriptedResponse::status(200)
        }],
        ScriptedResponse::status(204),
    )
    .await;
    let result = executor_for(&mock)
        .execute_outcome(
            &context(ModerationAction::Lockdown, None),
            &ActionOutcome::LockedDown {
                channel_id: CHANNEL.to_owned(),
            },
        )
        .await;
    assert!(
        result.is_err(),
        "unreadable channel produced {result:?}; {} wire requests",
        mock.requests().len()
    );
    assert_eq!(
        mock.requests().len(),
        1,
        "the read stays on the wire; no mutation follows"
    );
    mock.shutdown().await;
}

// Finding 3: an @everyone entry with non-numeric masks is unreadable too.
#[tokio::test]
async fn non_numeric_overwrite_masks_refuse_without_mutation() {
    let channel_body = serde_json::json!({
        "id": CHANNEL,
        "permission_overwrites": [
            {"id": GUILD, "type": 0, "allow": 12345, "deny": "0"}
        ],
    });
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, channel_body)],
        ScriptedResponse::status(204),
    )
    .await;
    let result = executor_for(&mock)
        .execute_outcome(
            &context(ModerationAction::Lockdown, None),
            &ActionOutcome::LockedDown {
                channel_id: CHANNEL.to_owned(),
            },
        )
        .await;
    assert!(
        result.is_err(),
        "non-string masks produced {result:?}; {} wire requests",
        mock.requests().len()
    );
    assert_eq!(mock.requests().len(), 1, "no mutation follows");
    mock.shutdown().await;
}

// Finding 3 follow-up: every overwrite row must carry a readable
// identity before it can count as "not @everyone". A malformed row is
// unreadable state — refuse with the one read on the wire, never PUT a
// fabricated zero-mask overwrite.
#[tokio::test]
async fn malformed_overwrite_rows_refuse_without_mutation() {
    let cases: Vec<(&str, serde_json::Value)> = vec![
        (
            "missing type",
            serde_json::json!([{"id": GUILD, "allow": "0", "deny": "0"}]),
        ),
        (
            "string type",
            serde_json::json!([{"id": GUILD, "type": "role", "allow": "0", "deny": "0"}]),
        ),
        (
            "numeric id",
            serde_json::json!([{"id": 2222, "type": 0, "allow": "0", "deny": "0"}]),
        ),
        ("null row", serde_json::json!([null])),
        (
            "missing id",
            serde_json::json!([{"type": 0, "allow": "0", "deny": "0"}]),
        ),
        // Finding 3 identity validation: string shape is not identity —
        // empty, nonnumeric, zero, and overflowing ids are unreadable.
        (
            "empty id",
            serde_json::json!([{"id": "", "type": 0, "allow": "0", "deny": "0"}]),
        ),
        (
            "nonnumeric id",
            serde_json::json!([{"id": "not-a-snowflake", "type": 0, "allow": "0", "deny": "0"}]),
        ),
        (
            "zero id",
            serde_json::json!([{"id": "0", "type": 0, "allow": "0", "deny": "0"}]),
        ),
        (
            "overflowing id",
            serde_json::json!([{"id": "18446744073709551616", "type": 0, "allow": "0", "deny": "0"}]),
        ),
    ];
    for (name, overwrites) in cases {
        let channel_body = serde_json::json!({
            "id": CHANNEL,
            "permission_overwrites": overwrites,
        });
        let mock = MockRest::start(
            vec![ScriptedResponse::json(200, channel_body)],
            ScriptedResponse::status(204),
        )
        .await;
        let result = executor_for(&mock)
            .execute_outcome(
                &context(ModerationAction::Lockdown, None),
                &ActionOutcome::LockedDown {
                    channel_id: CHANNEL.to_owned(),
                },
            )
            .await;
        assert!(
            result.is_err(),
            "{name} produced {result:?}; {} wire requests",
            mock.requests().len()
        );
        assert_eq!(
            mock.requests().len(),
            1,
            "{name}: the read stays on the wire; no mutation follows"
        );
        mock.shutdown().await;
    }
}

// Finding 3 follow-up (control): valid rows for other targets still count
// as proven-absent and lock down — only *malformed* rows refuse.
#[tokio::test]
async fn valid_non_target_rows_still_lock_down() {
    let channel_body = serde_json::json!({
        "id": CHANNEL,
        "permission_overwrites": [
            {"id": "9999", "type": 0, "allow": "0", "deny": "0"},
            {"id": GUILD, "type": 1, "allow": "0", "deny": "0"},
        ],
    });
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, channel_body)],
        ScriptedResponse::status(204),
    )
    .await;
    executor_for(&mock)
        .execute_outcome(
            &context(ModerationAction::Lockdown, None),
            &ActionOutcome::LockedDown {
                channel_id: CHANNEL.to_owned(),
            },
        )
        .await
        .expect("valid non-target rows are proven-absent and lock down");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2, "read then PUT");
    assert_eq!(reqs[1].method, "PUT");
    mock.shutdown().await;
}

// Finding 3 identity validation: the read compares normalized snowflakes,
// not raw strings, so a noncanonical caller guild id ("02222") still finds
// the existing @everyone row ("2222") and preserves its masks instead of
// writing a fabricated zero-mask overwrite.
#[tokio::test]
async fn noncanonical_guild_id_preserves_existing_masks() {
    let channel_body = serde_json::json!({
        "id": CHANNEL,
        "permission_overwrites": [
            {"id": GUILD, "type": 0, "allow": "1024", "deny": "64"},
        ],
    });
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, channel_body)],
        ScriptedResponse::status(204),
    )
    .await;
    let exec = ModerationExecution {
        guild_id: format!("0{GUILD}"),
        channel_id: Some(CHANNEL.to_owned()),
        action: ModerationAction::Lockdown,
        target_user_id: None,
        reason: REASON.to_owned(),
        duration_seconds: None,
        count: None,
        seconds: None,
    };
    executor_for(&mock)
        .execute_outcome(
            &exec,
            &ActionOutcome::LockedDown {
                channel_id: CHANNEL.to_owned(),
            },
        )
        .await
        .expect("noncanonical guild id resolves to the same target");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2, "read then PUT");
    assert_eq!(reqs[1].method, "PUT");
    let body: serde_json::Value =
        serde_json::from_slice(&reqs[1].body).expect("overwrite body is JSON");
    assert_eq!(
        body["allow"],
        serde_json::json!("1024"),
        "existing allow bits preserved"
    );
    assert_eq!(
        body["deny"],
        serde_json::json!("2112"),
        "existing deny bits preserved with the send bit set"
    );
    mock.shutdown().await;
}

// Finding 3 (control): a proven-absent @everyone entry still writes the
// lockdown masks — only *unreadable* state refuses.
#[tokio::test]
async fn proven_absent_overwrite_still_locks_down() {
    let channel_body = serde_json::json!({
        "id": CHANNEL,
        "permission_overwrites": [],
    });
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, channel_body)],
        ScriptedResponse::status(204),
    )
    .await;
    executor_for(&mock)
        .execute_outcome(
            &context(ModerationAction::Lockdown, None),
            &ActionOutcome::LockedDown {
                channel_id: CHANNEL.to_owned(),
            },
        )
        .await
        .expect("proven-absent overwrite locks down");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2, "read then PUT");
    assert_eq!(reqs[1].method, "PUT");
    mock.shutdown().await;
}

// Finding 4: unmodeled permission bits (1 << 48, absent from the pinned
// model) round-trip on the wire instead of serializing as "0".
#[tokio::test]
async fn overwrite_preserves_unmodeled_permission_bits() {
    let mask = (1u64 << 48).to_string();
    assert_eq!(
        twilight_model::guild::Permissions::all().bits() & (1u64 << 48),
        0,
        "bit 48 is genuinely unmodeled in pinned twilight-model"
    );
    let mock = MockRest::start(vec![], ScriptedResponse::status(204)).await;
    executor_for(&mock)
        .put_everyone_overwrite(CHANNEL, GUILD, &mask, "2048", REASON)
        .await
        .expect("overwrite write succeeds");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value =
        serde_json::from_slice(&reqs[0].body).expect("overwrite body is JSON");
    assert_eq!(body["allow"], serde_json::json!(mask));
    assert_eq!(body["deny"], serde_json::json!("2048"));
    mock.shutdown().await;
}

// Finding 5: the stored audit nonce (`oa_...`, 25 chars, never numeric)
// posts with enforcement.
#[tokio::test]
async fn audit_string_nonce_posts_with_enforcement() {
    let nonce = two_bot_core::audit::delivery_nonce("regression-entry");
    assert_eq!(nonce.len(), 25, "delivery nonce fills the nonce ceiling");
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(201, serde_json::json!({"id": "99"})),
    )
    .await;
    let call = ChannelCall::PostMessage {
        channel_id: CHANNEL.to_owned(),
        content: "audit entry".to_owned(),
        nonce: Some(nonce.clone()),
    };
    let outcome = executor_for(&mock)
        .execute_channel(&call)
        .await
        .expect("audit string nonce must be sendable");
    assert!(matches!(
        outcome,
        two_bot_discord::ChannelCallOutcome::Posted { .. }
    ));
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).expect("post body is JSON");
    assert_eq!(body["nonce"], serde_json::json!(nonce));
    assert_eq!(body["enforce_nonce"], serde_json::json!(true));
    assert_eq!(
        body["allowed_mentions"]["parse"],
        serde_json::json!([]),
        "mention suppression rides along"
    );
    mock.shutdown().await;
}

// Finding 5: numeric nonces keep the numeric wire shape, also enforced.
#[tokio::test]
async fn numeric_nonce_posts_with_enforcement() {
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(201, serde_json::json!({"id": "99"})),
    )
    .await;
    executor_for(&mock)
        .post_message(CHANNEL, "audit entry", Some(123))
        .await
        .expect("numeric nonce posts");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).expect("post body is JSON");
    assert_eq!(body["nonce"], serde_json::json!(123));
    assert_eq!(body["enforce_nonce"], serde_json::json!(true));
    mock.shutdown().await;
}

// Finding 6: purge returns the observed deletion count, not the requested
// one (Discord listed 3 of the 10 asked for).
#[tokio::test]
async fn purge_outcome_uses_actual_affected_count() {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(
            200,
            serde_json::json!([{"id": "11"}, {"id": "12"}, {"id": "13"}]),
        )],
        ScriptedResponse::status(204),
    )
    .await;
    let result = executor_for(&mock)
        .execute_outcome(
            &context(ModerationAction::Purge, Some(10)),
            &ActionOutcome::Purged {
                channel_id: CHANNEL.to_owned(),
                affected: 10,
            },
        )
        .await
        .expect("purge succeeds");
    assert_eq!(mock.requests().len(), 2, "list then one bulk delete");
    assert_eq!(
        result,
        ActionOutcome::Purged {
            channel_id: CHANNEL.to_owned(),
            affected: 3,
        }
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn purge_rejects_unreadable_history_without_deleting_valid_subset() {
    let mut bodies = vec![Vec::new(), b"[{".to_vec()];
    bodies.extend(
        [
            serde_json::json!(null),
            serde_json::json!({}),
            serde_json::json!([{}]),
            serde_json::json!([{"id":11}]),
            serde_json::json!([{"id":"0"}]),
            serde_json::json!([{"id":"invalid"}]),
            serde_json::json!([{"id":"18446744073709551616"}]),
            serde_json::json!([{"id":"011"}]),
            serde_json::json!([{"id":"11"}, {"id":"invalid"}]),
        ]
        .iter()
        .map(|body| body.to_string().into_bytes()),
    );
    for body in bodies {
        let mock = MockRest::start(
            vec![ScriptedResponse {
                body,
                ..ScriptedResponse::status(200)
            }],
            ScriptedResponse::status(204),
        )
        .await;
        let result = executor_for(&mock).purge(CHANNEL, 2, REASON).await;
        assert!(matches!(result, Err(DiscordError::Unavailable(_))));
        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "only the read-only list was sent");
        assert_eq!(requests[0].method, "GET");
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn purge_accepts_a_valid_empty_history_without_deletion() {
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, serde_json::json!([]))],
        ScriptedResponse::status(500),
    )
    .await;
    assert_eq!(
        executor_for(&mock).purge(CHANNEL, 2, REASON).await.unwrap(),
        0
    );
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(mock.requests()[0].method, "GET");
    mock.shutdown().await;
}

// Finding 8: the legacy ceiling is UTF-16 units — 1001 astral chars are
// 2002 units and must be rejected with zero wire calls.
#[tokio::test]
async fn message_ceiling_counts_utf16_units() {
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(201, serde_json::json!({"id": "99"})),
    )
    .await;
    let text = "😀".repeat(1001);
    assert_eq!(text.encode_utf16().count(), 2002);
    let result = executor_for(&mock).post_message(CHANNEL, &text, None).await;
    assert!(
        result.is_err(),
        "2002 UTF-16 units produced {result:?}; {} wire requests",
        mock.requests().len()
    );
    assert!(mock.requests().is_empty(), "zero wire calls");
    mock.shutdown().await;
}

// Finding 8 (boundary): exactly 2000 UTF-16 units still posts.
#[tokio::test]
async fn message_ceiling_boundary_posts() {
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(201, serde_json::json!({"id": "99"})),
    )
    .await;
    let text = format!("{}😀", "a".repeat(1998));
    assert_eq!(text.encode_utf16().count(), 2000);
    executor_for(&mock)
        .post_message(CHANNEL, &text, None)
        .await
        .expect("exactly 2000 UTF-16 units posts");
    assert_eq!(mock.requests().len(), 1);
    mock.shutdown().await;
}
