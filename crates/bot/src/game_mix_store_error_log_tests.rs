//! Builder-owned pin for the classified `/game-mix` store-failure log.
//!
//! Review on the staff game-mix slice requires the aggregate-read `Err` arm
//! to emit a bounded `db_class` warn (an operator signal) instead of
//! discarding the store error. The protected Test Author acceptance modules
//! stay untouched, so this module pins the warn here: drop the report table
//! on a disposable database, invoke through the gateway, and assert the
//! process-wide capture holds the classified event while the reply stays the
//! generic private retry guidance with no member identity.

#![cfg(test)]

use super::*;
use std::collections::HashMap;
use two_bot_testsupport::TestDatabase;

const COMMAND: &str = "game-mix";
const MEMBER_ID: u64 = 445_110_998_877_665;
const MEMBER_NAME: &str = "GameMixStoreLogIdentityProbe";

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit disposable test bootstrap");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database; no application credential fallback")
}

fn staff() -> Interaction {
    let mut interaction = from_member(
        slash(COMMAND, Some(CHANNEL), Vec::new()),
        MEMBER_ID,
        Permissions::MANAGE_GUILD,
    );
    interaction.id = Id::new(990_221);
    interaction.token = "game-mix-store-log-test-token".to_owned();
    let member = interaction.member.as_mut().expect("guild member fixture");
    member.user.as_mut().expect("fixture user").name = MEMBER_NAME.to_owned();
    interaction.user = Some(user(MEMBER_ID, false));
    interaction
}

async fn dispatch(pool: PgPool, interaction: Interaction) -> Vec<RestRequest> {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(RouterGates {
            configured_guild: Some(GUILD),
            ..gates(false, false)
        }),
        GUILD,
        false,
    );
    let guard = runtime.dispatch_guard();
    runtime.initialize_custom_commands_for_test(
        crate::gateway_commands::GatewayCommandConfig::from_map_for_activation(
            GUILD,
            &HashMap::from([("TWO_AUTOMATIONS".to_owned(), "0".to_owned())]),
            false,
        )
        .expect("test gateway configuration"),
    );
    let _ = runtime.dispatch(&interaction_event(interaction));
    wait_for(
        || (0..6).all(|lane| runtime.lane_in_flight(lane) == 0),
        "game-mix store-log gateway work settles",
    )
    .await;
    let requests = mock.requests();
    drop(guard);
    drop(runtime);
    mock.shutdown().await;
    requests
}

fn completion(requests: &[RestRequest]) -> serde_json::Value {
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|request| request.path.ends_with("/callback"))
        .collect();
    assert_eq!(callbacks.len(), 1, "one initial response: {requests:?}");
    let initial: serde_json::Value =
        serde_json::from_slice(&callbacks[0].body).expect("callback JSON");
    assert_eq!(initial["data"]["flags"], 64, "private acknowledgement");
    if initial["type"] == 5 {
        assert_eq!(requests.len(), 2, "only defer plus original-message edit");
        assert_eq!(requests[1].method, "PATCH");
        serde_json::from_slice::<serde_json::Value>(&requests[1].body).expect("completion JSON")
    } else {
        assert_eq!(initial["type"], 4);
        assert_eq!(requests.len(), 1, "no channel post, DM or second response");
        initial["data"].clone()
    }
}

#[tokio::test]
async fn store_failure_emits_classified_warn_and_keeps_generic_reply() {
    let db = database().await;
    // Only this disposable fixture is damaged; no shared schema is touched.
    sqlx::query("DROP TABLE game_activity_guild_aggregates")
        .execute(db.pool())
        .await
        .expect("simulate unavailable report store");
    let capture = crate::tracing_capture::global();
    let start = capture.text().len();
    let requests = dispatch(db.pool().clone(), staff()).await;
    let logged = capture.text().split_off(start);
    db.close().await.expect("dispose only owned database");

    // The classified operator signal: deleting the warn turns both red.
    assert!(
        logged.contains("game_mix_aggregate_read_failed"),
        "classified failure token is logged"
    );
    assert!(
        logged.contains("game-mix aggregate read failed"),
        "classified failure message is logged"
    );
    assert!(
        !logged.contains(&MEMBER_ID.to_string()),
        "failure log carries no member identity"
    );
    assert!(
        !logged.contains(MEMBER_NAME),
        "failure log carries no member name"
    );

    // Staff still get the generic private retry guidance, never a report.
    let body = completion(&requests);
    assert_eq!(body["allowed_mentions"]["parse"], serde_json::json!([]));
    let text = body["content"].as_str().expect("reply content");
    assert!(
        text.to_lowercase().contains("try again"),
        "fixed retry guidance"
    );
    assert!(!text.contains("```"), "store failure fabricates no report");
    assert!(!text.contains(MEMBER_NAME), "reply carries no member name");
}
