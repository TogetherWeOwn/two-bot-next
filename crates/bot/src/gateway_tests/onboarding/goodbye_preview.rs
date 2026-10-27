#![cfg(test)]
//! Protected socket-level witness: a real shard receives a slash, never a leave.
use super::*;

#[tokio::test]
#[ignore = "requires disposable agent-testdb; real loopback gateway and REST"]
async fn goodbye_preview_shard_dispatch_completes_ephemerally_without_a_member_leaving() {
    let db = TestDb::new().await;
    let mock = MockRest::with_responder(|request| {
        match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/api/v10/interactions/333/goodbye-preview-socket/callback") => {
                ScriptedResponse::status(204)
            }
            ("PATCH", "/api/v10/webhooks/1111/goodbye-preview-socket/messages/@original") => {
                ScriptedResponse::json(200, json!({"id":"98"}))
            }
            _ => ScriptedResponse::status(403),
        }
    })
    .await;
    let result = bounded(async {
        let mut fixture = gateway(false).await;
        let runner = spawn_onboarding(&db, &mock, &fixture.mock.url, "session").await;
        assert_eq!(fixture.mock.authentication().await["op"], 2);
        wait_sequence(&db.store, 1).await;
        fixture.send(json!({"op":0,"s":2,"t":"INTERACTION_CREATE","d":{
            "application_id":"1111","authorizing_integration_owners":{"0":GUILD},
            "id":"333","token":"goodbye-preview-socket","type":2,"version":1,"guild_id":GUILD,
            "member":{"user":{"id":"77","username":"staff-not-a-departure","discriminator":"0","bot":false},
                "permissions":"32","roles":[],"deaf":false,"mute":false,"flags":0},
            "data":{"id":"444","name":"goodbye-preview","type":1,"options":[]}
        }})).await;
        let edit = wait_request(&mock, "PATCH", "/api/v10/webhooks/1111/goodbye-preview-socket/messages/@original").await;
        wait_sequence(&db.store, 2).await;
        let requests = mock.requests();
        assert_eq!(requests.len(), 2, "defer and edit only, with no member/channel lookup or public post");
        assert!(requests[0].path.ends_with("/callback"));
        let defer: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(defer["type"], 5);
        assert_eq!(defer["data"]["flags"], 64);
        let body: Value = serde_json::from_slice(&edit.body).unwrap();
        let text = body["content"].as_str().unwrap();
        let first = text.lines().next().unwrap_or_default().to_lowercase();
        assert!(first.contains("preview") && first.contains("not a delivery"));
        assert!(text.contains("<#13>"));
        let expected = "**Preview \\*member\\* @\u{200b}everyone <\u{200b}@765432109876543210>** left the server (was here 7 days). Their messages and voice history stay on the books.";
        assert!(text.ends_with(expected), "real shard must reach live goodbye renderer: {text}");
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM events").fetch_one(&db.pool).await.unwrap();
        assert_eq!(rows, 0, "no member-leave or onboarding funnel facts were fabricated");
        assert!(posts(&mock).is_empty());
        runner.stop().await;
    }).await;
    cleanup(db, Some(mock), result).await;
}
