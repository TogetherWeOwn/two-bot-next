#![cfg(test)]

//! Protected behavioral acceptance: claim/close tell members what happened.
//! Real runtime + store, guarded disposable database, loopback Discord only.
//! The builder must not edit this file. No exact success-copy spelling is pinned.
//! Failure accounting uses one structured `ticket claim marker failed` event
//! with `failed=1`; this is a count, not a log of the Discord response body.
use super::*;
use crate::{
    command_runtime::CommandRuntime,
    discord_test_common::{MockRest, RestRequest, ScriptedResponse},
};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use two_bot_testsupport::TestDatabase;

async fn database() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        eprintln!("SKIP claim/close truth DB acceptance: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated disposable test database"),
    )
}

fn runtime(pool: PgPool, mock: &MockRest) -> Arc<TicketRuntime> {
    let executor =
        ActionExecutor::with_proxy("ticket-test-token".into(), Some(mock.origin())).unwrap();
    let runtime = Arc::new(
        TicketRuntime::new(
            pool,
            executor,
            TicketConfig {
                guild_id: "100".into(),
                category_id: "200".into(),
                panel_channel_id: "700".into(),
                staff_role_id: "300".into(),
                cooldown_seconds: COOLDOWN_SECONDS,
            },
            HashMap::new(),
        )
        .unwrap(),
    );
    runtime.set_bot_id(400);
    runtime
}

fn button(action: TicketAction, actor: &str, id: &str) -> Interaction {
    let custom_id = match action {
        TicketAction::Open => TICKET_OPEN_ID,
        TicketAction::Claim => TICKET_CLAIM_ID,
        TicketAction::Close => TICKET_CLOSE_ID,
    };
    serde_json::from_value(json!({
        "application_id":"400", "authorizing_integration_owners":{"0":"100"},
        "id":id, "token":format!("ticket-test-{id}"), "type":3, "version":1,
        "guild_id":"100", "channel":{"id":"600","type":0,"name":"ticket-member","permissions":"0"},
        "data":{"custom_id":custom_id,"component_type":2},
        "member":{
            "roles":["300"], "permissions":"0", "deaf":false,"mute":false,"flags":0,
            "user":{"id":actor,"username":"@everyone <@&300> staff","discriminator":"0","avatar":null}
        }
    }))
    .expect("synthetic wire-format ticket button")
}

fn channel() -> Value {
    let common = (Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::READ_MESSAGE_HISTORY)
        .bits();
    json!({"id":"600","guild_id":"100","type":0,"parent_id":"200",
    "permission_overwrites":[
        {"id":"400","type":1,"allow":(common | Permissions::MANAGE_CHANNELS.bits()).to_string(),"deny":"0"},
        {"id":"500","type":1,"allow":common.to_string(),"deny":"0"}
    ]})
}

async fn active(runtime: &TicketRuntime) -> Ticket {
    assert!(matches!(
        runtime.store.reserve("ticket", "500", 0, 0).await.unwrap(),
        OpenResult::Created(_)
    ));
    runtime.store.record_channel("ticket", "600").await.unwrap();
    runtime.store.activate("ticket", "600").await.unwrap()
}

fn member_reply(result: Result<String>) -> String {
    match result {
        Ok(reply) => reply,
        Err(error) => error.reply(),
    }
}

fn marker_posts(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests
        .iter()
        .filter(|request| {
            request.method == "POST" && request.path.ends_with("/channels/600/messages")
        })
        .collect()
}

fn assert_mentions_suppressed(body: &Value) {
    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
    for key in ["users", "roles"] {
        assert!(
            body["allowed_mentions"][key].is_null() || body["allowed_mentions"][key] == json!([]),
            "no explicit ping allowlist in {body}"
        );
    }
    assert_ne!(body["allowed_mentions"]["replied_user"], json!(true));
}

async fn wait_for_reply(mock: &MockRest, id: &str) -> Value {
    let suffix = format!("/ticket-test-{id}/messages/@original");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(request) = mock
                .requests()
                .into_iter()
                .find(|request| request.method == "PATCH" && request.path.ends_with(&suffix))
            {
                return serde_json::from_slice(&request.body).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("shared router must finish its deferred reply")
}

#[tokio::test]
async fn committed_claim_posts_named_marker_after_commit_and_replay_posts_nothing() {
    let Some(db) = database().await else { return };
    let (mock, gate) = MockRest::start_gated(
        vec![ScriptedResponse::json(200, json!({"id":"900"}))],
        ScriptedResponse::status(500),
    )
    .await;
    let tickets = runtime(db.pool().clone(), &mock);
    active(&tickets).await;
    let peer = db.independent_pool().await.unwrap();
    let observer = TicketStore::new(peer.clone(), "100".into()).unwrap();
    let interaction = button(TicketAction::Claim, "501", "801");
    let worker = Arc::clone(&tickets);
    let event = interaction.clone();
    let mut task = tokio::spawn(async move { worker.execute(&event, TicketAction::Claim).await });
    // If the post is missing, the baseline completes instead of reaching the
    // gate. Never manufacture a timeout and call it behavioral RED.
    let (reached_post, first_result) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            _ = gate.wait_for_request() => (true, None),
            result = &mut task => (false, Some(result.unwrap())),
        }
    })
    .await
    .expect("claim must finish or reach its marker post");
    let visible_at_post = observer.get("ticket").await.unwrap().unwrap();
    gate.release();
    let first_result = match first_result {
        Some(result) => result,
        None => task.await.unwrap(),
    };
    let first_ok = first_result.is_ok();
    let first_reply = member_reply(first_result);
    let _replay = tickets.execute(&interaction, TicketAction::Claim).await;
    let stored = observer.get("ticket").await.unwrap().unwrap();
    let requests = mock.requests();
    peer.close().await;
    db.close().await.unwrap();
    mock.shutdown().await;

    assert!(first_ok, "claim must succeed: {first_reply}");
    assert_eq!(visible_at_post.claimed_by.as_deref(), Some("501"));
    assert_eq!(stored.claimed_by.as_deref(), Some("501"));
    assert_eq!(stored.status, TicketStatus::Open);
    assert_eq!(stored.created_at, 0);
    assert!(
        reached_post,
        "a committed claim must post a visible channel marker"
    );
    let posts = marker_posts(&requests);
    assert_eq!(posts.len(), 1, "replayed claim must not post twice");
    let body: Value = serde_json::from_slice(&posts[0].body).unwrap();
    let content = body["content"].as_str().expect("visible marker content");
    assert!(
        content.contains("<@501>"),
        "marker must name the claimant: {content}"
    );
    assert!(content.to_lowercase().contains("claim"));
    assert_mentions_suppressed(&body);
}

#[tokio::test]
async fn failed_marker_is_counted_once_without_undoing_claim_or_retrying_on_replay() {
    let Some(db) = database().await else { return };
    let capture = crate::tracing_capture::global();
    let start = capture.text().len();
    let mock = MockRest::start(
        vec![],
        ScriptedResponse::json(
            403,
            json!({
                "message":"SYNTHETIC_REMOTE_BODY_MUST_NOT_BE_LOGGED","code":50013
            }),
        ),
    )
    .await;
    let tickets = runtime(db.pool().clone(), &mock);
    active(&tickets).await;
    let interaction = button(TicketAction::Claim, "501", "801");
    let result = tickets.execute(&interaction, TicketAction::Claim).await;
    let succeeded = result.is_ok();
    let reply = member_reply(result);
    let _replay = tickets.execute(&interaction, TicketAction::Claim).await;
    let stored = tickets.store.get("ticket").await.unwrap().unwrap();
    let requests = mock.requests();
    let logs = capture.text()[start..].to_owned();
    db.close().await.unwrap();
    mock.shutdown().await;

    assert!(succeeded, "a marker rejection never un-claims: {reply}");
    assert_eq!(stored.claimed_by.as_deref(), Some("501"));
    assert_eq!(stored.status, TicketStatus::Open);
    assert_eq!(
        marker_posts(&requests).len(),
        1,
        "403 is a single bounded attempt; replay adds none"
    );
    let counted: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("ticket claim marker failed"))
        .collect();
    assert_eq!(
        counted.len(),
        1,
        "one count event for one failed post: {logs}"
    );
    assert!(
        counted[0]
            .split_whitespace()
            .any(|field| field == "failed=1"),
        "failure has a numeric count: {logs}"
    );
    assert!(!logs.contains("SYNTHETIC_REMOTE_BODY_MUST_NOT_BE_LOGGED"));
}

#[tokio::test]
async fn second_staff_member_learns_claimant_in_a_mention_suppressed_router_reply() {
    let Some(db) = database().await else { return };
    let mock = MockRest::with_responder(|request| {
        if request.path.ends_with("/callback") {
            ScriptedResponse::status(204)
        } else if request.method == "PATCH" || request.path.ends_with("/channels/600/messages") {
            ScriptedResponse::json(200, json!({"id":"900"}))
        } else {
            ScriptedResponse::status(500)
        }
    })
    .await;
    let tickets = runtime(db.pool().clone(), &mock);
    active(&tickets).await;
    let commands = CommandRuntime::with_tickets(
        db.pool().clone(),
        tickets.executor.clone(),
        Arc::clone(&tickets),
    );
    commands
        .on_interaction(&button(TicketAction::Claim, "501", "801"))
        .await;
    let _first_reply = wait_for_reply(&mock, "801").await;
    commands
        .on_interaction(&button(TicketAction::Claim, "502", "802"))
        .await;
    let reply = wait_for_reply(&mock, "802").await;
    let stored = tickets.store.get("ticket").await.unwrap().unwrap();
    let requests = mock.requests();
    db.close().await.unwrap();
    mock.shutdown().await;

    assert_eq!(stored.claimed_by.as_deref(), Some("501"));
    let content = reply["content"].as_str().unwrap();
    assert!(
        content.to_lowercase().contains("already") && content.to_lowercase().contains("claim"),
        "specific ownership refusal: {content}"
    );
    assert!(
        content.contains("<@501>"),
        "name the stored winner, not the new actor: {content}"
    );
    assert!(!content.contains("<@502>"));
    assert!(!content.to_lowercase().contains("or not open"));
    assert_mentions_suppressed(&reply);
    assert_eq!(
        marker_posts(&requests).len(),
        1,
        "losing claim posts no marker"
    );
    let defers: Vec<Value> = requests
        .iter()
        .filter(|request| request.path.ends_with("/callback"))
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(defers.len(), 2);
    assert!(defers
        .iter()
        .all(|body| body["type"] == 5 && body["data"]["flags"] == 64));
}

#[tokio::test]
async fn closing_ticket_refuses_as_not_open_even_when_it_has_a_claimant() {
    let Some(db) = database().await else { return };
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let tickets = runtime(db.pool().clone(), &mock);
    active(&tickets).await;
    tickets.store.claim("ticket", "501").await.unwrap();
    let closing = tickets.store.begin_close("ticket", 10).await.unwrap();
    let result = tickets
        .execute(
            &button(TicketAction::Claim, "502", "802"),
            TicketAction::Claim,
        )
        .await;
    let refused = result.is_err();
    let reply = member_reply(result);
    let stored = tickets.store.get("ticket").await.unwrap().unwrap();
    let requests = mock.requests();
    db.close().await.unwrap();
    mock.shutdown().await;

    assert!(refused);
    assert_eq!(stored, closing);
    assert!(requests.is_empty(), "a state refusal must not call Discord");
    let lower = reply.to_lowercase();
    assert!(
        lower.contains("not open") || lower.contains("closing"),
        "name the lifecycle reason: {reply}"
    );
    assert!(
        !lower.contains("already claimed"),
        "closing is not an ownership conflict: {reply}"
    );
    assert!(!lower.contains("recover"));
}

async fn close_case(truncated: bool) -> Option<(String, String, i64, Vec<RestRequest>, Ticket)> {
    let db = database().await?;
    let content = if truncated {
        "😀".repeat(110_000)
    } else {
        "private support detail".into()
    };
    let history = json!([{
        "id":"900", "timestamp":"1970-01-01T00:00:00.900Z",
        "author":{"id":"500","username":"member","discriminator":"0"},
        "content":content, "attachments":[]
    }]);
    let mock = MockRest::with_responder(move |request| {
        if request.method == "GET" && request.path.ends_with("/channels/600") {
            ScriptedResponse::json(200, channel())
        } else if request.method == "GET" && request.path.contains("/channels/600/messages?") {
            ScriptedResponse::json(200, history.clone())
        } else if (request.method == "PUT" && request.path.ends_with("/permissions/500"))
            || (request.method == "DELETE" && request.path.ends_with("/channels/600"))
        {
            ScriptedResponse::status(204)
        } else {
            ScriptedResponse::status(500)
        }
    })
    .await;
    let tickets = runtime(db.pool().clone(), &mock);
    active(&tickets).await;
    tickets.store.claim("ticket", "501").await.unwrap();
    let interaction = button(TicketAction::Close, "501", "803");
    let result = tickets.execute(&interaction, TicketAction::Close).await;
    let succeeded = result.is_ok();
    let reply = member_reply(result);
    let stored = tickets.store.get("ticket").await.unwrap().unwrap();
    let transcript: (String, String, String, String) = sqlx::query_as(
        "SELECT content, created_at, purge_after, claimed_by FROM ticket_transcripts WHERE ticket_id = 'ticket'"
    ).fetch_one(db.pool()).await.unwrap();
    let replay = tickets.execute(&interaction, TicketAction::Close).await;
    let requests = mock.requests();
    db.close().await.unwrap();
    mock.shutdown().await;

    assert!(succeeded, "successful saved close: {reply}");
    assert!(replay.is_err(), "completed close replay remains a refusal");
    assert_eq!(transcript.3, "501");
    let retention = two_bot_core::funnel::parse_iso_millis(&transcript.2).unwrap()
        - two_bot_core::funnel::parse_iso_millis(&transcript.1).unwrap();
    Some((reply, transcript.0, retention, requests, stored))
}

fn assert_retention_without_retrieval_promise(reply: &str, retention: i64) {
    assert_eq!(retention, 7_776_000_000, "no retention policy change");
    let lower = reply.to_lowercase();
    assert!(
        lower.contains("90 days"),
        "name the actual retention ceiling: {reply}"
    );
    assert!(
        lower.contains("up to") || lower.contains("at most"),
        "not a guaranteed minimum: {reply}"
    );
    // No transcript read API is being built by this slice. A documented
    // retrieval decision can be reviewed separately; do not invent a link.
    for promise in [
        "download",
        "retrieve",
        "request a copy",
        "available at",
        "view your transcript",
    ] {
        assert!(
            !lower.contains(promise),
            "no nonexistent read path: {reply}"
        );
    }
    assert!(!reply.contains("private support detail"));
    assert!(!reply.contains('😀'));
}

#[tokio::test]
async fn truncated_close_reply_discloses_loss_and_preserves_retention_and_replay() {
    let Some((reply, transcript, retention, requests, stored)) = close_case(true).await else {
        return;
    };
    assert!(
        transcript.ends_with("[transcript truncated]"),
        "fixture must really be truncated"
    );
    assert!(
        reply.to_lowercase().contains("truncat"),
        "member must learn the saved transcript is incomplete: {reply}"
    );
    assert_retention_without_retrieval_promise(&reply, retention);
    assert_eq!(stored.status, TicketStatus::Closed);
    assert_eq!(stored.claimed_by.as_deref(), Some("501"));
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path.contains("/messages?"))
            .count(),
        1,
        "close replay must not recapture history"
    );
}

#[tokio::test]
async fn complete_close_reply_never_falsely_warns_of_truncation_or_promises_retrieval() {
    let Some((reply, transcript, retention, requests, stored)) = close_case(false).await else {
        return;
    };
    assert!(transcript.contains("private support detail"));
    assert!(!transcript.contains("[transcript truncated]"));
    assert!(
        !reply.to_lowercase().contains("truncat"),
        "complete capture must not claim data loss: {reply}"
    );
    assert_retention_without_retrieval_promise(&reply, retention);
    assert_eq!(stored.status, TicketStatus::Closed);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .count(),
        1
    );
}

#[tokio::test]
async fn unauthorized_claim_and_close_still_refuse_before_storage_or_discord() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_local")
        .unwrap();
    let tickets = runtime(pool, &mock);
    for action in [TicketAction::Claim, TicketAction::Close] {
        let mut interaction = button(action, "500", "804");
        interaction.member.as_mut().unwrap().roles.clear();
        let result = tickets.execute(&interaction, action).await;
        assert!(matches!(
            result,
            Err(Failure::Store(StoreError::Domain(TicketError::StaffOnly)))
        ));
    }
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}
