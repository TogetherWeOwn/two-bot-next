//! Fail-open acceptance: lost Discord access must refuse, never read as absent.
//!
//! Protected failing-first tests (paired build card implements the fix; the
//! Builder must not edit this file). `exec_one` currently maps `403 | 404`
//! and 5xx-retry-exhausted to `Ok(None)`, so callers treating `None` as
//! "nothing to do" silently skip work the bot lost access to. The contract
//! pinned here:
//!
//! - 403 is a refusal: `Err` whose message names 403 (distinct error class).
//! - 404 stays `Ok(None)`: genuine absence, exactly as today.
//! - 5xx on every attempt (retry budget spent) is `Err`, never `Ok(None)`.
//! - 2xx still returns the decoded body (healthy path unchanged).
//!
//! The refusal shape follows the existing `delete_channel` precedent
//! (`Err` with an `HTTP 403` / `HTTP 5xx retry exhausted` message); the
//! Builder may use a new `RestError` variant instead, as long as the `Err`
//! surfaces and the message names the failing status. The archived-threads
//! walk keeps its own legacy walk-end contract inside
//! `list_all_archived_threads` (covered by the in-crate walk tests); these
//! tests pin the single-model readers (`active_threads`, `guild`), which
//! must propagate the refusal.
//!
//! Hermetic loopback: every test serves a fixed status from 127.0.0.1 and
//! asserts the verdict plus the wire-attempt count (403/404 answer once,
//! 5xx spends all five attempts, then refuses).
//! python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test fail_open_refusal_acceptance

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use twilight_model::channel::thread::ThreadsListing;
use twilight_model::id::marker::GuildMarker;
use twilight_model::id::Id;
use two_bot_cutover::rest::RestClient;

fn reason_for(status: u16) -> &'static str {
    match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Internal Server Error",
    }
}

/// Serve one fixed `(status, body)` for every request; returns the proxy
/// origin plus the wire-attempt counter.
fn serve(status: u16, body: String) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut socket) = stream else { break };
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf);
            seen.fetch_add(1, Ordering::SeqCst);
            let _ = write!(
                socket,
                "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                reason_for(status),
                body.len()
            );
        }
    });
    (base, count)
}

fn guild() -> Id<GuildMarker> {
    Id::new(2222)
}

fn empty_listing_body() -> String {
    serde_json::to_string(&ThreadsListing {
        has_more: Some(false),
        members: Vec::new(),
        threads: Vec::new(),
    })
    .expect("fixture serializes")
}

#[tokio::test]
async fn forbidden_thread_listing_is_refusal_not_absence() {
    let (base, count) = serve(403, "{}".to_owned());
    let rest = RestClient::with_proxy("t".into(), Some(base));
    let result = rest.active_threads(guild()).await;
    let error = result.expect_err("403 must refuse, not read as absent");
    assert!(
        error.to_string().contains("403"),
        "refusal names the lost access: {error}"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "a refusal answers once, without retry"
    );
}

#[tokio::test]
async fn forbidden_guild_fetch_is_refusal_not_absence() {
    let (base, _) = serve(403, "{}".to_owned());
    let rest = RestClient::with_proxy("t".into(), Some(base));
    let result = rest.guild(guild()).await;
    let error = result.expect_err("403 must refuse, not read as absent");
    assert!(
        error.to_string().contains("403"),
        "refusal names the lost access: {error}"
    );
}

#[tokio::test]
async fn missing_thread_listing_stays_absent() {
    // Control: genuine 404 absence keeps today's `Ok(None)` contract.
    let (base, count) = serve(404, "{}".to_owned());
    let rest = RestClient::with_proxy("t".into(), Some(base));
    let listing = rest
        .active_threads(guild())
        .await
        .expect("404 is not an error");
    assert!(listing.is_none(), "404 stays genuine absence");
    assert_eq!(count.load(Ordering::SeqCst), 1, "absence answers once");
}

#[tokio::test]
async fn exhausted_retries_are_an_error_not_absence() {
    // Five attempts (initial + four backoff retries at 500/1000/2000/4000 ms),
    // then refusal: never a silent `None`.
    let (base, count) = serve(500, "{}".to_owned());
    let rest = RestClient::with_proxy("t".into(), Some(base));
    let result = rest.active_threads(guild()).await;
    let error = result.expect_err("5xx-exhausted must refuse, not read as absent");
    let message = error.to_string();
    assert!(
        message.contains("5xx") || message.contains("exhausted"),
        "exhaustion names the spent budget: {message}"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        5,
        "the full retry budget is spent before refusing"
    );
}

#[tokio::test]
async fn healthy_thread_listing_still_returns_the_body() {
    // Control: the well-formed path decodes exactly as today.
    let (base, _) = serve(200, empty_listing_body());
    let rest = RestClient::with_proxy("t".into(), Some(base));
    let listing = rest
        .active_threads(guild())
        .await
        .expect("2xx is not an error")
        .expect("2xx decodes to a listing");
    assert!(listing.threads.is_empty());
    assert_eq!(listing.has_more, Some(false));
}
