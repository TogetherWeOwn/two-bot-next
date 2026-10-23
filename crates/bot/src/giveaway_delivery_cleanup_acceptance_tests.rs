//! Protected acceptance: a confirmed POST must not leave an enabled orphan
//! when attaching its message id fails. Only the Discord transport is mocked;
//! dispatch, admission, persistence and governed execution stay real.
//!
//! The disposable schema's UPDATE trigger fails only the attachment write,
//! after admission and delivery. A nontransactional sequence proves that seam
//! was reached even though the injected SQL error rolls back its transaction.
//! Missing cleanup, cleaning the wrong message, leaving the button enabled,
//! blindly rePOSTing, or claiming cleanup after a 403 must fail these tests.
#![cfg(test)]

use super::*;

const DELIVERED_ID: &str = "8800000000000000001";
const CREATE_TOKEN: &str = "gw-attach-failure";

struct AttachmentFailure {
    requests: Vec<RestRequest>,
    attachment_attempted: bool,
    persisted_ids: Vec<String>,
}

async fn attachment_failure(cleanup_status: u16) -> AttachmentFailure {
    let db = TestDb::new().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(
        "CREATE SEQUENCE giveaway_attachment_attempt;
         CREATE FUNCTION reject_giveaway_attachment() RETURNS trigger
         LANGUAGE plpgsql AS $$
         BEGIN
           PERFORM nextval('giveaway_attachment_attempt');
           RAISE EXCEPTION 'fixture attachment write rejected';
         END;
         $$;
         CREATE TRIGGER reject_giveaway_attachment
         BEFORE UPDATE OF message_id ON giveaways
         FOR EACH ROW
         WHEN (NEW.message_id <> OLD.message_id AND NEW.message_id <> '')
         EXECUTE FUNCTION reject_giveaway_attachment();",
    ))
    .execute(&db.pool)
    .await
    .expect("install attachment-only failure in own disposable schema");

    let (runtime, mock) = db_runtime_script(
        &db,
        vec![
            RestResponse::status(200), // ephemeral defer
            RestResponse {
                status: 200,
                body: Some(format!(r#"{{"id":"{DELIVERED_ID}"}}"#)),
                delay: Duration::ZERO,
            }, // confirmed public message POST
            RestResponse::status(cleanup_status), // governed cleanup, not rePOST
            RestResponse::status(200), // ephemeral completion
        ],
    )
    .await;
    let interaction = giveaway_create(74_001, CREATE_TOKEN, Permissions::MANAGE_GUILD, Vec::new());
    runtime.on_interaction(&interaction).await;
    // Discord redelivery must not create a second message after a known receipt
    // and failed attachment. This is not a new command from the administrator.
    runtime.on_interaction(&interaction).await;

    let evidence = AttachmentFailure {
        requests: mock.requests(),
        attachment_attempted: sqlx::query_scalar(
            "SELECT is_called FROM giveaway_attachment_attempt",
        )
        .fetch_one(&db.pool)
        .await
        .expect("attachment trigger evidence survives its transaction rollback"),
        persisted_ids: sqlx::query_scalar("SELECT message_id FROM giveaways WHERE guild_id = $1")
            .bind(GUILD_S)
            .fetch_all(&db.pool)
            .await
            .expect("inspect unattached rows"),
    };
    mock.shutdown().await;
    db.close().await;
    evidence
}

fn cleanup_requests(requests: &[RestRequest]) -> Vec<&RestRequest> {
    let channel_path = format!("/api/v10/channels/{CHANNEL}/messages/{DELIVERED_ID}");
    let followup_path = format!("/api/v10/webhooks/1111/{CREATE_TOKEN}/messages/{DELIVERED_ID}");
    requests
        .iter()
        .filter(|request| {
            matches!(request.method.as_str(), "PATCH" | "DELETE")
                && (request.path == channel_path || request.path == followup_path)
        })
        .collect()
}

fn assert_attachment_failure_reached(evidence: &AttachmentFailure) {
    let posts = message_posts(&evidence.requests);
    assert_eq!(
        posts.len(),
        1,
        "never blindly rePOST after a confirmed receipt"
    );
    assert!(
        buttons(&posts[0])
            .iter()
            .any(|(custom_id, _)| opaque_id(custom_id).is_some()),
        "the delivered message actually carried a giveaway entry button"
    );
    assert!(
        evidence.attachment_attempted,
        "the failure must be post-delivery attachment, not admission or parsing"
    );
    assert!(
        evidence.persisted_ids.iter().all(|id| id != DELIVERED_ID),
        "fixture must prevent persistence of the confirmed message id"
    );
}

fn assert_neutralizing_payload(request: &RestRequest, posted: &Value) {
    if request.method == "DELETE" {
        return; // removing the known message also neutralizes its entry button
    }
    let patch: Value = serde_json::from_slice(&request.body).expect("cleanup JSON");
    let rows = patch["components"]
        .as_array()
        .expect("cleanup explicitly replaces components; omission keeps live buttons");
    for row in rows {
        for component in row["components"].as_array().expect("action row") {
            if component["custom_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("gw:"))
            {
                assert_eq!(
                    component["disabled"], true,
                    "no enabled orphan entry button"
                );
            }
        }
    }
    let text = message_text(&patch);
    assert!(
        !text.is_empty(),
        "cleanup explains that entry is unavailable"
    );
    assert_ne!(
        text,
        message_text(posted),
        "do not retain the live-entry copy"
    );
    assert_eq!(patch["allowed_mentions"]["parse"], serde_json::json!([]));
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn giveaway_attachment_error_neutralizes_the_confirmed_message_without_repost() {
    let evidence = attachment_failure(200).await;
    assert_attachment_failure_reached(&evidence);
    let cleanup = cleanup_requests(&evidence.requests);
    assert_eq!(
        cleanup.len(),
        1,
        "attachment failed after POST: govern one cleanup of the known delivered id"
    );
    assert_neutralizing_payload(cleanup[0], &message_posts(&evidence.requests)[0]);
    let post_index = evidence
        .requests
        .iter()
        .position(|request| request.method == "POST" && request.path.ends_with("/messages"))
        .expect("confirmed message post");
    let cleanup_index = evidence
        .requests
        .iter()
        .position(|request| std::ptr::eq(request, cleanup[0]))
        .expect("cleanup request");
    assert!(
        cleanup_index > post_index,
        "cleanup follows confirmed delivery"
    );
    let reply = reply_for(&evidence.requests, CREATE_TOKEN);
    assert!(
        reply.found && reply.ephemeral,
        "ephemeral failure completion"
    );
    assert!(reply.text.to_lowercase().contains("giveaway"));
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn giveaway_attachment_cleanup_forbidden_requires_truthful_inspection_guidance() {
    let evidence = attachment_failure(403).await;
    assert_attachment_failure_reached(&evidence);
    let cleanup = cleanup_requests(&evidence.requests);
    assert_eq!(
        cleanup.len(),
        1,
        "even when cleanup fails, attempt it through the governed executor exactly once"
    );
    assert_neutralizing_payload(cleanup[0], &message_posts(&evidence.requests)[0]);
    let reply = reply_for(&evidence.requests, CREATE_TOKEN);
    assert!(
        reply.found && reply.ephemeral,
        "ephemeral inspection guidance"
    );
    let text = reply.text.to_lowercase();
    assert!(text.contains("giveaway"));
    assert!(
        text.contains("inspect") || text.contains("manually"),
        "failed cleanup needs manual inspection guidance: {text}"
    );
    assert!(
        text.contains(DELIVERED_ID),
        "identify the orphan for inspection: {text}"
    );
    assert!(
        [
            "failed",
            "unable",
            "could not",
            "couldn't",
            "uncertain",
            "cannot"
        ]
        .iter()
        .any(|word| text.contains(word)),
        "state the cleanup failure or uncertainty: {text}"
    );
    for false_success in [
        "cleanup succeeded",
        "successfully cleaned",
        "message was disabled",
        "message was removed",
        "giveaway created.",
    ] {
        assert!(
            !text.contains(false_success),
            "false cleanup success: {text}"
        );
    }
}
