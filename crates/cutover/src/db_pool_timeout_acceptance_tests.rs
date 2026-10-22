#![cfg(test)]
//! Acceptance: cutover pool opens refuse a hung database within a documented
//! bound (P1 pool-timeout triple).
//!
//! BUILDER CONTRACT: this file is frozen. Build on the acceptance branch,
//! keep every test here verbatim, and make the suite green by changing
//! production code only:
//!
//! - Set a documented `acquire_timeout` in `pool_options`
//!   (`crates/cutover/src/db.rs`); `voice_pool_options` inherits it through
//!   the shared constructor. The bound must sit comfortably under
//!   [`REFUSAL_BOUND`] (in-repo precedents are 2-10 s); the tests assert
//!   refusal, never an exact value. A voice exception needs a pinned test and
//!   a return to the Test Author: this file cannot be edited on the build.
//! - Keep the existing `statement_timeout` leg and the redacted
//!   `InvalidArgument("database connection failed")` connect mapping.
//! - Either fix shape passes: an eager connect that refuses with the existing
//!   mapping, or a lazy pool whose first checkout refuses with
//!   `sqlx::Error::PoolTimedOut`.
//! - Update the two existing acquire-default pins in `pool_options_tests`
//!   (`nonvoice_pool_preserves_sqlx_defaults` and
//!   `voice_pool_keeps_a_warm_connection_and_checks_liveness`) to the new
//!   documented bound: they pin today's missing timeout. Do not edit this
//!   file or its `#[cfg(test)]` wiring in `db.rs`.
//!
//! Suite map (all offline: the hung stub is a loopback listener that accepts
//! and never speaks Postgres; no database, no Discord):
//!
//! - hung-database refusal (RED now: the pool carries the 30 s sqlx default
//!   acquire timeout, so the 20 s bound fires first).
//! - voice inheritance pin (green now and after: both constructors share it).
//! - statement-leg and empty-URL controls (green now and after).

use std::time::Duration;

use super::*;
use two_bot_core::database_tls::TlsPolicy;

/// Outer bound for the hung-database refusal. sqlx 0.9 defaults to a 30 s
/// acquire timeout, so a pool without the triple hangs past this bound; any
/// documented acquire bound under 20 s (precedents: 2-10 s) refuses inside it.
const REFUSAL_BOUND: Duration = Duration::from_secs(20);

/// A hung Postgres: accepts TCP and never speaks the protocol.
async fn start_hung_db() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind hung-database stub");
    let port = listener
        .local_addr()
        .expect("hung-database stub address")
        .port();
    let task = tokio::spawn(async move {
        loop {
            let accepted = listener.accept().await;
            let Ok((socket, _)) = accepted else {
                break;
            };
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                drop(socket);
            });
        }
    });
    (port, task)
}

fn hung_url(port: u16) -> String {
    format!("postgres://hung_probe:fixture-hung-password@127.0.0.1:{port}/hung_db")
}

#[tokio::test]
async fn hung_database_refuses_within_bound_instead_of_hanging() {
    let (port, hung) = start_hung_db().await;
    let url = hung_url(port);
    let outcome = tokio::time::timeout(REFUSAL_BOUND, async {
        match connect_with_tls(&url, 1, true, TlsPolicy::LocalOnly).await {
            Err(error) => assert!(
                matches!(
                    &error,
                    sqlx::Error::InvalidArgument(message)
                        if message.as_str() == "database connection failed"
                ),
                "eager connect must reuse the existing redacted refusal, got {error:?}"
            ),
            Ok(db) => {
                let error = sqlx::query_scalar::<_, i32>("SELECT 1")
                    .fetch_one(db.pool())
                    .await
                    .unwrap_err();
                db.close().await;
                assert!(
                    matches!(error, sqlx::Error::PoolTimedOut),
                    "first checkout must refuse with PoolTimedOut, got {error:?}"
                );
            }
        }
    })
    .await;
    hung.abort();
    let bound_secs = REFUSAL_BOUND.as_secs();
    assert!(
        outcome.is_ok(),
        "hung database must refuse within {bound_secs} s, not hang"
    );
}

#[test]
fn voice_pool_inherits_the_documented_acquire_bound() {
    assert_eq!(
        voice_pool_options(DB_POOL_MAX_DEFAULT).get_acquire_timeout(),
        pool_options(DB_POOL_MAX_DEFAULT).get_acquire_timeout(),
        "voice_pool_options must inherit pool_options' acquire bound"
    );
}

#[test]
fn statement_timeout_leg_stays_on_cutover_connect() {
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/db.rs"))
        .expect("read db.rs");
    assert!(
        source.contains("statement_timeout"),
        "cutover connect must keep its statement_timeout leg"
    );
}

#[tokio::test]
async fn empty_url_still_refused_without_connecting() {
    let error = connect_with_tls("", 1, true, TlsPolicy::LocalOnly)
        .await
        .unwrap_err();
    assert!(matches!(error, sqlx::Error::InvalidArgument(_)));
}
