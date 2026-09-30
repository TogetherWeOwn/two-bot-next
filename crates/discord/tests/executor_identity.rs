//! Identity reads through the actual shared executor and loopback REST double.
#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::json;
use two_bot_discord::ActionExecutor;

#[tokio::test]
async fn resolves_bot_and_application_without_gateway_ready() {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id": "99", "bot": true})),
            ScriptedResponse::json(200, json!({"id": "11"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let executor = ActionExecutor::with_proxy("mock-token".into(), Some(mock.origin())).unwrap();
    assert_eq!(executor.current_identity().await.unwrap(), (99, 11));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/api/v10/users/@me");
    assert_eq!(requests[1].path, "/api/v10/oauth2/applications/@me");
    mock.shutdown().await;
}

#[tokio::test]
async fn refused_or_non_bot_identity_stops_before_application_read() {
    for response in [
        ScriptedResponse::status(401),
        ScriptedResponse::json(200, json!({"id": "99", "bot": false})),
        ScriptedResponse::json(200, json!({"bot": true})),
    ] {
        let mock = MockRest::start(vec![response], ScriptedResponse::status(500)).await;
        let executor =
            ActionExecutor::with_proxy("mock-token".into(), Some(mock.origin())).unwrap();
        assert!(executor.current_identity().await.is_err());
        assert_eq!(mock.requests().len(), 1);
        mock.shutdown().await;
    }
}
