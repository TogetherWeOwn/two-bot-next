#![cfg(test)]
//! Registration and error-contract tests for the supervised LFG sweep.
//!
//! The protected jobs-seam acceptance suite proves the sweep does real work
//! under `jobs::supervise`; these tests pin the registration fence (the env
//! gate narrowed by boot identity, never widened) and the job's error
//! contract (database failures stay database failures, identity failures
//! stay REST). Same offline doubles as the dispatch tests (mock REST with a
//! lazy refused pool), so every case runs without a database server.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::json;
use two_bot_core::database_tls::TlsPolicy;
use two_bot_core::FeatureGates;
use two_bot_discord::executor::ActionExecutor;
use two_bot_discord::lfg_interactions::{LfgError, LfgInteractions};

use crate::{
    activation::fixtures,
    discord_test_common::{MockRest, RestRequest, ScriptedResponse},
    gateway::ensure_crypto_provider,
    jobs::ErrorClass,
    lfg_jobs::{register, register_fenced, NAME},
    website_jobs::{admission_pool_with_tls, Context},
};

const GUILD: &str = "2222";
const BOT: u64 = 1111;
const OFFLINE_URL: &str = "postgres://fixture:fixture-password@127.0.0.1:1/fixture?sslmode=disable";

fn gates(announcements: Option<&str>) -> FeatureGates {
    let mut vars = HashMap::new();
    if let Some(value) = announcements {
        vars.insert("TWO_ANNOUNCEMENTS".to_owned(), value.to_owned());
    }
    FeatureGates::from_map(&vars).expect("fixed gate fixtures parse")
}

fn executor(mock: &MockRest) -> ActionExecutor {
    ensure_crypto_provider();
    ActionExecutor::with_proxy("lfg-jobs-test-token".to_owned(), Some(mock.origin()))
        .expect("loopback executor builds")
}

fn offline_pool() -> sqlx::PgPool {
    admission_pool_with_tls(OFFLINE_URL, TlsPolicy::LocalOnly).expect("lazy refused pool builds")
}

fn wired_context(mock: &MockRest) -> Context {
    Context::for_tests(
        OFFLINE_URL.to_owned(),
        offline_pool(),
        executor(mock),
        GUILD.to_owned(),
    )
}

#[tokio::test]
async fn registration_parks_without_announcements_and_registers_with_them() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!([]))).await;
    let context = Arc::new(wired_context(&mock));
    assert!(register_fenced(gates(None), context.clone(), &fixtures::staging()).is_none());
    assert!(register_fenced(gates(Some("0")), context.clone(), &fixtures::staging()).is_none());
    let job = register_fenced(gates(Some("1")), context.clone(), &fixtures::staging());
    assert_eq!(job.as_ref().map(|job| job.name), Some(NAME));
    for (_label, activation) in fixtures::refused() {
        let parked = register_fenced(gates(Some("1")), context.clone(), &activation);
        assert!(parked.is_none());
    }
    mock.shutdown().await;
}

#[tokio::test]
async fn register_follows_the_process_environment() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!([]))).await;
    let context = Arc::new(wired_context(&mock));
    let activation = fixtures::staging();
    let expected = matches!(FeatureGates::from_env(), Ok(gates) if activation.constrain_features(gates).announcements);
    assert_eq!(register(context, &activation).is_some(), expected);
    mock.shutdown().await;
}

#[tokio::test]
async fn action_reports_database_errors_through_the_job_contract() {
    let mock = MockRest::with_responder(|request: &RestRequest| {
        let _ = request;
        ScriptedResponse::json(200, json!({"id": "1111", "bot": true}))
    })
    .await;
    let context = Arc::new(wired_context(&mock));
    let job = register_fenced(gates(Some("1")), context, &fixtures::staging())
        .expect("staging registers the sweep");
    assert_eq!((job.action)().await, Err(ErrorClass::Database));
    mock.shutdown().await;
}

#[tokio::test]
async fn action_reports_identity_failures_as_rest_errors() {
    let mock = MockRest::with_responder(|request: &RestRequest| {
        let _ = request;
        ScriptedResponse::json(200, json!({"unusable": true}))
    })
    .await;
    let context = Arc::new(wired_context(&mock));
    let job = register_fenced(gates(Some("1")), context, &fixtures::staging())
        .expect("staging registers the sweep");
    assert_eq!((job.action)().await, Err(ErrorClass::Rest));
    mock.shutdown().await;
}

#[tokio::test]
async fn sweep_rejects_unusable_batches_without_touching_the_database() {
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!([]))).await;
    let service = LfgInteractions::new(offline_pool());
    let now = time::OffsetDateTime::now_utc();
    let processed = service
        .sweep_lifecycle(&executor(&mock), GUILD, BOT, now, 0)
        .await;
    assert_eq!(processed.expect("an empty batch is not an error"), 0);
    let refused = service
        .sweep_lifecycle(&executor(&mock), GUILD, 0, now, 3)
        .await;
    assert!(matches!(refused, Err(LfgError::Uncertain)));
    assert!(mock.requests().is_empty());
    mock.shutdown().await;
}
