//! Hermetic loopback regressions for bounded cutover member pagination.
//! python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test rest_member_pagination
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use twilight_model::id::Id;
use two_bot_cutover::rest::{RestClient, RestError};

fn member(id: u64) -> String {
    format!(
        r#"{{"user":{{"id":"{id}","username":"u","discriminator":"0","avatar":null}},"roles":[],"joined_at":"2024-01-01T00:00:00.000000+00:00","deaf":false,"mute":false,"flags":0}}"#
    )
}

fn page(ids: impl Iterator<Item = u64>) -> String {
    format!("[{}]", ids.map(member).collect::<Vec<_>>().join(","))
}

/// Serve `respond(after)` for every request; returns (base, request counter).
fn serve(respond: impl Fn(u64) -> String + Send + 'static) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let line = req.lines().next().unwrap_or_default();
            let after = line
                .split("after=")
                .nth(1)
                .and_then(|r| r.split(['&', ' ']).next())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            c.fetch_add(1, Ordering::SeqCst);
            let body = respond(after);
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (base, count)
}

fn guild() -> Id<twilight_model::id::marker::GuildMarker> {
    Id::new(1)
}

#[tokio::test]
async fn advancing_pages_return_every_member_once() {
    let (base, count) = serve(|after| match after {
        0 => page(1..=1000),
        1000 => page(1001..=2000),
        _ => page(2001..=2010),
    });
    let rest = RestClient::with_proxy("t".into(), Some(base));
    let m = rest
        .fetch_all_members(guild())
        .await
        .expect("ok")
        .expect("some");
    assert_eq!(m.len(), 2010);
    let mut ids: Vec<u64> = m.iter().map(|x| x.user.id.get()).collect();
    ids.dedup();
    assert_eq!(ids.len(), 2010);
    assert_eq!(count.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn repeated_full_page_is_rejected_after_bounded_requests() {
    let (base, count) = serve(|_| page(1..=1000));
    let rest = RestClient::with_proxy("t".into(), Some(base));
    let err = rest.fetch_all_members(guild()).await.expect_err("stalled");
    assert!(matches!(err, RestError::MemberCursorStalled), "{err}");
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn page_ceiling_is_an_error_not_a_partial_roster() {
    let (base, count) = serve(|after| page(after + 1..=after + 1000));
    let rest = RestClient::with_proxy("t".into(), Some(base));
    let err = rest
        .fetch_all_members_bounded(guild(), 2, usize::MAX)
        .await
        .expect_err("ceiling");
    assert!(
        matches!(err, RestError::MemberCeilingExceeded { pages: 2, .. }),
        "{err}"
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn member_ceiling_is_an_error_not_a_partial_roster() {
    let (base, _) = serve(|after| page(after + 1..=after + 1000));
    let rest = RestClient::with_proxy("t".into(), Some(base));
    let err = rest
        .fetch_all_members_bounded(guild(), 100, 1500)
        .await
        .expect_err("ceiling");
    assert!(
        matches!(err, RestError::MemberCeilingExceeded { members: 2000, .. }),
        "{err}"
    );
}
