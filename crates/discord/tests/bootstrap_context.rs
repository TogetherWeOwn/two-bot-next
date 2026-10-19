//! Custom-command bootstrap metadata reads over the shared REST executor.
//! All traffic stays on the existing loopback-only MockRest; no gateway,
//! database, live Discord, or command publication is involved.

#[allow(dead_code)]
mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::{MockRest, ScriptedResponse, APP_ID, GUILD_ID};
use serde_json::json;
use two_bot_core::send_admission::{
    AdmissionError, AdmissionFuture, AdmissionPermit, SendAdmission, SendCompletion, SendCooldown,
    TokenKey,
};
use two_bot_discord::{ActionExecutor, DiscordError, MODERATION_TIMEOUT_MS};

const APPLICATION_PATH: &str = "/api/v10/applications/@me";
const GUILD_PATH: &str = "/api/v10/guilds/2222";

fn executor_for(mock: &MockRest) -> ActionExecutor {
    ActionExecutor::with_proxy("bootstrap-test-token".to_owned(), Some(mock.origin()))
        .expect("executor builds against loopback")
}

fn assert_gets(mock: &MockRest, executor: &ActionExecutor, paths: &[&str]) {
    let requests = mock.requests();
    assert_eq!(requests.len(), paths.len(), "no retries or extra calls");
    assert_eq!(executor.requests(), paths.len() as u64);
    for (request, path) in requests.iter().zip(paths) {
        assert_eq!(request.method, "GET", "bootstrap never writes");
        assert_eq!(request.path, *path);
        assert!(request.body.is_empty(), "metadata reads have no body");
    }
}

fn malformed_response() -> ScriptedResponse {
    let mut response = ScriptedResponse::status(200);
    response.body = b"private-bootstrap-body-not-json".to_vec();
    response
}

#[tokio::test]
async fn valid_bootstrap_metadata_uses_shared_get_routes_without_writes() {
    let name = "  TWO Community Café  ";
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id": APP_ID.to_string()})),
            ScriptedResponse::json(200, json!({"id": GUILD_ID.to_string(), "name": name})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let executor = executor_for(&mock);

    assert_eq!(executor.current_application_id().await.unwrap(), APP_ID);
    assert_eq!(executor.guild_name(GUILD_ID).await.unwrap(), name);
    assert_gets(&mock, &executor, &[APPLICATION_PATH, GUILD_PATH]);
    mock.shutdown().await;
}

#[tokio::test]
async fn invalid_or_absent_application_metadata_returns_only_a_fixed_error() {
    let mut responses = vec![ScriptedResponse::status(200), malformed_response()];
    responses.extend(
        [
            json!(null),
            json!([]),
            json!({}),
            json!({"id": null}),
            json!({"id": APP_ID}),
            json!({"id": true}),
            json!({"id": ""}),
            json!({"id": "0"}),
            json!({"id": "-1"}),
            json!({"id": "18446744073709551616"}),
            json!({"id": "private-application-metadata"}),
        ]
        .into_iter()
        .map(|doc| ScriptedResponse::json(200, doc)),
    );
    let count = responses.len();
    let mock = MockRest::start(responses, ScriptedResponse::status(500)).await;
    let executor = executor_for(&mock);

    for _ in 0..count {
        assert_eq!(
            executor.current_application_id().await.unwrap_err(),
            DiscordError::Rejected("invalid application metadata".to_owned()),
            "response content must never appear in the error"
        );
    }
    assert_gets(&mock, &executor, &vec![APPLICATION_PATH; count]);
    mock.shutdown().await;
}

#[tokio::test]
async fn invalid_mismatched_or_absent_guild_metadata_returns_only_a_fixed_error() {
    let mut responses = vec![ScriptedResponse::status(200), malformed_response()];
    responses.extend(
        [
            json!(null),
            json!([]),
            json!({}),
            json!({"name": "private-guild-name"}),
            json!({"id": null, "name": "private-guild-name"}),
            json!({"id": GUILD_ID, "name": "private-guild-name"}),
            json!({"id": "", "name": "private-guild-name"}),
            json!({"id": "0", "name": "private-guild-name"}),
            json!({"id": "3333", "name": "private-guild-name"}),
            json!({"id": "-1", "name": "private-guild-name"}),
            json!({"id": "18446744073709551616", "name": "private-guild-name"}),
            json!({"id": "private-guild-id", "name": "private-guild-name"}),
            json!({"id": GUILD_ID.to_string()}),
            json!({"id": GUILD_ID.to_string(), "name": null}),
            json!({"id": GUILD_ID.to_string(), "name": 123}),
            json!({"id": GUILD_ID.to_string(), "name": ""}),
            json!({"id": GUILD_ID.to_string(), "name": " \t\n"}),
        ]
        .into_iter()
        .map(|doc| ScriptedResponse::json(200, doc)),
    );
    let count = responses.len();
    let mock = MockRest::start(responses, ScriptedResponse::status(500)).await;
    let executor = executor_for(&mock);

    for _ in 0..count {
        assert_eq!(
            executor.guild_name(GUILD_ID).await.unwrap_err(),
            DiscordError::Rejected("invalid guild metadata".to_owned()),
            "neither the malformed identity nor the name may leak"
        );
    }
    assert_gets(&mock, &executor, &vec![GUILD_PATH; count]);
    mock.shutdown().await;
}

#[tokio::test]
async fn zero_guild_input_refuses_before_io() {
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let executor = executor_for(&mock);

    assert_eq!(
        executor.guild_name(0).await.unwrap_err(),
        DiscordError::Rejected("bad guild id".to_owned())
    );
    assert_gets(&mock, &executor, &[]);
    mock.shutdown().await;
}

#[tokio::test]
async fn only_200_is_accepted_and_status_errors_never_retry_or_leak_bodies() {
    let statuses = [201, 204, 403, 404, 429, 503];
    let responses = statuses
        .iter()
        .flat_map(|status| {
            [
                ScriptedResponse::json(*status, json!({"id": APP_ID.to_string()})),
                ScriptedResponse::json(
                    *status,
                    json!({"id": GUILD_ID.to_string(), "name": "private-status-body"}),
                ),
            ]
        })
        .collect();
    let mock = MockRest::start(responses, ScriptedResponse::status(500)).await;
    let executor = executor_for(&mock);

    tokio::time::timeout(Duration::from_secs(2), async {
        for status in statuses {
            // Shared executor taxonomy: documented client refusals reject;
            // any other unexpected status (including 2xx that is not the
            // accepted 200) is unavailable, never a retry with the body.
            let expected = match status {
                429 => DiscordError::RateLimited,
                201 | 204 | 503 => DiscordError::Unavailable(format!("Discord returned {status}")),
                _ => DiscordError::Rejected(format!("Discord refused the request with {status}")),
            };
            assert_eq!(
                executor.current_application_id().await.unwrap_err(),
                expected
            );
            assert_eq!(executor.guild_name(GUILD_ID).await.unwrap_err(), expected);
        }
    })
    .await
    .expect("status errors return without retry waits");
    let paths: Vec<_> = statuses
        .iter()
        .flat_map(|_| [APPLICATION_PATH, GUILD_PATH])
        .collect();
    assert_gets(&mock, &executor, &paths);
    mock.shutdown().await;
}

#[tokio::test]
async fn both_metadata_reads_use_the_shared_five_second_abort() {
    let delay = Duration::from_millis(MODERATION_TIMEOUT_MS + 1_000);
    let mock = MockRest::start(vec![], ScriptedResponse::status(200).delayed(delay)).await;
    let executor = executor_for(&mock);

    let (application, guild) = tokio::time::timeout(delay, async {
        tokio::join!(
            executor.current_application_id(),
            executor.guild_name(GUILD_ID)
        )
    })
    .await
    .expect("both reads abort before the delayed response");
    assert_eq!(application.unwrap_err(), DiscordError::Timeout);
    assert_eq!(guild.unwrap_err(), DiscordError::Timeout);
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(executor.requests(), 2);
    assert!(requests.iter().all(|request| request.method == "GET"));
    assert!(requests.iter().all(|request| request.body.is_empty()));
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path == APPLICATION_PATH)
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path == GUILD_PATH)
            .count(),
        1
    );
    mock.shutdown().await;
}

const TOKEN: &str = "bootstrap-test-token";

/// A lane that refuses its first `blocked` admissions and then admits, the way
/// a held single-flight row clears once its holder completes.
#[derive(Debug)]
struct HeldThenFree {
    key: TokenKey,
    blocked: u32,
    calls: AtomicU32,
}

struct Release;

impl SendCompletion for Release {
    fn complete(
        self: Box<Self>,
        _cooldown: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        Box::pin(async { Ok(()) })
    }
}

impl SendAdmission for HeldThenFree {
    fn token_key(&self) -> &TokenKey {
        &self.key
    }

    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let admitted = call >= self.blocked;
        Box::pin(async move {
            if admitted {
                Ok(AdmissionPermit::new(Box::new(Release)))
            } else {
                Err(AdmissionError::Blocked)
            }
        })
    }
}

fn governed_executor(mock: &MockRest, blocked: u32) -> (ActionExecutor, Arc<HeldThenFree>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let lane = Arc::new(HeldThenFree {
        key: TokenKey::for_bot_token(TOKEN).unwrap(),
        blocked,
        calls: AtomicU32::new(0),
    });
    let executor = ActionExecutor::with_admission(
        TOKEN.to_owned(),
        Some(mock.origin()),
        Arc::clone(&lane) as Arc<dyn SendAdmission>,
    )
    .expect("governed executor builds against loopback");
    (executor, lane)
}

#[tokio::test]
async fn boot_reads_wait_out_a_held_lane_instead_of_failing_the_gateway() {
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"id": APP_ID.to_string()})),
            ScriptedResponse::json(200, json!({"id": GUILD_ID.to_string(), "name": "TWO"})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    // Three refusals ahead of the first read: a startup job still holds the
    // lane when the gateway's bootstrap asks for it.
    let (executor, lane) = governed_executor(&mock, 3);

    assert_eq!(executor.current_application_id().await.unwrap(), APP_ID);
    assert_eq!(executor.guild_name(GUILD_ID).await.unwrap(), "TWO");

    // Refused attempts never reached the wire, so no read was resent.
    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "one wire request per read");
    assert_eq!(requests[0].path, APPLICATION_PATH);
    assert_eq!(requests[1].path, GUILD_PATH);
    assert_eq!(
        lane.calls.load(Ordering::SeqCst),
        5,
        "3 refused + 2 admitted"
    );
    mock.shutdown().await;
}

#[tokio::test]
async fn boot_waiting_retries_only_the_pre_wire_refusal() {
    let mock = MockRest::start(
        vec![ScriptedResponse::status(403), ScriptedResponse::status(503)],
        ScriptedResponse::status(500),
    )
    .await;
    let (executor, lane) = governed_executor(&mock, 1);

    // The refusal is waited out once; the 403 that follows is final.
    assert_eq!(
        executor.current_application_id().await.unwrap_err(),
        DiscordError::Rejected("Discord refused the request with 403".to_owned())
    );
    assert_eq!(
        executor.guild_name(GUILD_ID).await.unwrap_err(),
        DiscordError::Unavailable("Discord returned 503".to_owned())
    );
    assert_eq!(mock.requests().len(), 2, "no status is retried");
    assert_eq!(
        lane.calls.load(Ordering::SeqCst),
        3,
        "1 refused + 2 admitted"
    );
    mock.shutdown().await;
}
