//! Acceptance tests: `/import` attachment download must spend ONE
//! ten-second budget on request headers AND the whole body together.
//!
//! Today `download_attachment` times out only `transport.request`
//! (voice_rooms.rs, the `tokio::time::timeout(Duration::from_secs(10), ...)`
//! around the request) and collects the `Limited` body afterwards with no
//! elapsed bound. A CDN response that returns valid 200 headers and then
//! stalls (or drips chunks) below the byte cap therefore leaves an
//! acknowledged `/import` running forever. The byte cap bounds size, not
//! time.
//!
//! Required production behaviour (builder):
//! - the existing ten-second budget covers the request, the status check
//!   and every body byte, measured from the start of the download; it is
//!   never restarted when headers arrive;
//! - a stalled or slow body past that budget fails with the same
//!   sanitized errors as today (`RoomHttpError::UnknownOutcome` for a
//!   timeout/transport failure, `RoomHttpError::InvalidRequest` for an
//!   oversized body or a refused URL, the existing `classify_response`
//!   result for a non-2xx status) and returns no partial bytes, so the
//!   bot handler can only take its "Could not download that file.
//!   Nothing was changed" branch: no preview, no confirmation token,
//!   no configuration write;
//! - HTTPS + `cdn.discordapp.com` allowlisting, credential-free requests,
//!   the `max_bytes` cap, exact-size success, status classification and
//!   no-redirect-following are all preserved.
//!
//! Test-only seam the builder must provide (this file is protected and
//! must not be edited to match a different seam): a private field
//! `cdn_origin: String` on `RoomHttp`. Production constructors set it to
//! `"https://cdn.discordapp.com"`. `download_attachment` still validates
//! the user-supplied URL (HTTPS, host `cdn.discordapp.com`) exactly as
//! today, then -- for the transport request only -- replaces the
//! scheme/authority with `cdn_origin`, preserving path and query. With
//! the production value the rewrite is the identity. These tests point
//! the field at a scripted loopback server, the same pattern
//! `voice_rooms_http_tests` already uses for the API `origin` through
//! the private `with_origin` constructor.
//!
//! Time control: loopback IO runs on live time; the clock is paused and
//! advanced only while both peers are parked (server holding the body
//! open, client waiting to read), so the ten-second production budget
//! can be spent instantly without wall-clock sleeps. Every download is
//! awaited behind a short real-time guard so a missing deadline FAILS
//! these tests instead of hanging them.

use super::*;
use std::net::SocketAddr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
};

/// A signed-attachment-shaped URL. The signature is fake fixture text;
/// the loopback server never leaves this machine and no live CDN or
/// Discord request is ever made.
const CDN_URL: &str =
    "https://cdn.discordapp.com/attachments/111/222/voice-config.json?ex=00&is=00&hm=fakefixture";

type Recorded = Arc<Mutex<Vec<String>>>;

fn recorded() -> Recorded {
    Arc::new(Mutex::new(Vec::new()))
}

/// Build the client under test against a loopback CDN origin. Goes
/// through the existing private constructor (HTTP loopback allowed),
/// then points the builder-provided `cdn_origin` seam at the scripted
/// server. The user-facing URL passed to `download_attachment` stays a
/// real `https://cdn.discordapp.com/...` URL, so production URL
/// validation is exercised unchanged.
fn api_for(addr: SocketAddr) -> RoomHttp {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut api = RoomHttp::with_origin(
        "loopback-only-test-token".to_owned(),
        "http://127.0.0.1/api/v10".to_owned(),
        true,
    )
    .unwrap();
    api.cdn_origin = format!("http://{addr}");
    api
}

async fn bind() -> (SocketAddr, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    (listener.local_addr().unwrap(), listener)
}

/// Accept one connection and read its request head (through the blank
/// line). Records the head so tests can assert on the request line and
/// on the absence of credentials.
async fn accept(listener: &TcpListener, heads: &Recorded) -> TcpStream {
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    heads
        .lock()
        .unwrap()
        .push(String::from_utf8_lossy(&head).into_owned());
    stream
}

async fn write(stream: &mut TcpStream, bytes: &[u8]) {
    stream.write_all(bytes).await.unwrap();
    stream.flush().await.unwrap();
}

fn chunk(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

const CHUNKED_200: &[u8] = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n";
const FINAL_CHUNK: &[u8] = b"0\r\n\r\n";

/// Spend fake time while both peers are parked, then return to live
/// time. Never call while loopback IO is in flight.
async fn advance_paused(spent: Duration) {
    tokio::time::pause();
    tokio::time::advance(spent).await;
    tokio::time::resume();
}

/// Await a spawned download behind a short real-time guard: if the
/// production deadline is missing the guard fires and the test fails
/// with this message instead of hanging.
async fn expect_finished(
    task: tokio::task::JoinHandle<Result<Vec<u8>, RoomHttpError>>,
) -> Result<Vec<u8>, RoomHttpError> {
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("download still running after its ten-second budget was spent")
        .unwrap()
}

#[tokio::test]
async fn headers_then_stalled_body_hits_the_ten_second_deadline() {
    let (addr, listener) = bind().await;
    let heads = recorded();
    let body_open = Arc::new(Notify::new());
    let server = {
        let heads = heads.clone();
        let body_open = body_open.clone();
        tokio::spawn(async move {
            let mut stream = accept(&listener, &heads).await;
            write(&mut stream, CHUNKED_200).await;
            write(&mut stream, &chunk(b"{\"partial\":")).await;
            body_open.notify_one();
            // Hold the body open forever; the test aborts this task.
            std::future::pending::<()>().await;
        })
    };
    let api = api_for(addr);
    let task = tokio::spawn(async move { api.download_attachment(CDN_URL, 1024).await });

    // Headers and a first chunk have arrived; both peers are parked.
    body_open.notified().await;
    advance_paused(Duration::from_secs(11)).await;

    assert_eq!(
        expect_finished(task).await,
        Err(RoomHttpError::UnknownOutcome),
        "a body stalled past the budget must be the sanitized download failure, with no bytes"
    );
    server.abort();
    let heads = heads.lock().unwrap();
    assert_eq!(heads.len(), 1, "exactly one download request");
    assert!(
        heads[0]
            .starts_with("GET /attachments/111/222/voice-config.json?ex=00&is=00&hm=fakefixture "),
        "path and signed query are preserved for the transport: {:?}",
        heads[0]
    );
    assert!(
        !heads[0].to_ascii_lowercase().contains("authorization"),
        "attachment downloads carry no bot credential: {:?}",
        heads[0]
    );
}

#[tokio::test]
async fn deadline_is_not_restarted_when_headers_arrive() {
    let (addr, listener) = bind().await;
    let heads = recorded();
    let accepted = Arc::new(Notify::new());
    let send_headers = Arc::new(Notify::new());
    let headers_sent = Arc::new(Notify::new());
    let server = {
        let heads = heads.clone();
        let accepted = accepted.clone();
        let send_headers = send_headers.clone();
        let headers_sent = headers_sent.clone();
        tokio::spawn(async move {
            let mut stream = accept(&listener, &heads).await;
            accepted.notify_one();
            send_headers.notified().await;
            write(&mut stream, CHUNKED_200).await;
            write(&mut stream, &chunk(b"{}")).await;
            headers_sent.notify_one();
            // Never finish the body; the test aborts this task.
            std::future::pending::<()>().await;
        })
    };
    let api = api_for(addr);
    let task = tokio::spawn(async move { api.download_attachment(CDN_URL, 1024).await });

    // Six seconds of the budget are spent waiting for headers...
    accepted.notified().await;
    advance_paused(Duration::from_secs(6)).await;
    send_headers.notify_one();
    headers_sent.notified().await;
    // ...and six more after they arrive: twelve from the start, only six
    // since headers. A budget restarted at headers would still have
    // four seconds left and must fail this test.
    advance_paused(Duration::from_secs(6)).await;

    assert_eq!(
        expect_finished(task).await,
        Err(RoomHttpError::UnknownOutcome),
        "one shared deadline from download start, never restarted at headers"
    );
    server.abort();
}

#[tokio::test]
async fn stalled_headers_hit_the_same_deadline() {
    let (addr, listener) = bind().await;
    let heads = recorded();
    let accepted = Arc::new(Notify::new());
    let server = {
        let heads = heads.clone();
        let accepted = accepted.clone();
        tokio::spawn(async move {
            let _stream = accept(&listener, &heads).await;
            accepted.notify_one();
            std::future::pending::<()>().await;
        })
    };
    let api = api_for(addr);
    let task = tokio::spawn(async move { api.download_attachment(CDN_URL, 1024).await });

    accepted.notified().await;
    advance_paused(Duration::from_secs(11)).await;

    assert_eq!(
        expect_finished(task).await,
        Err(RoomHttpError::UnknownOutcome),
        "the pre-headers phase keeps its existing timeout classification"
    );
    server.abort();
}

#[tokio::test]
async fn slow_chunks_within_the_shared_budget_succeed() {
    let (addr, listener) = bind().await;
    let heads = recorded();
    let first_sent = Arc::new(Notify::new());
    let send_second = Arc::new(Notify::new());
    let second_sent = Arc::new(Notify::new());
    let send_final = Arc::new(Notify::new());
    let server = {
        let heads = heads.clone();
        let first_sent = first_sent.clone();
        let send_second = send_second.clone();
        let second_sent = second_sent.clone();
        let send_final = send_final.clone();
        tokio::spawn(async move {
            let mut stream = accept(&listener, &heads).await;
            write(&mut stream, CHUNKED_200).await;
            write(&mut stream, &chunk(b"voice-")).await;
            first_sent.notify_one();
            send_second.notified().await;
            write(&mut stream, &chunk(b"config")).await;
            second_sent.notify_one();
            send_final.notified().await;
            write(&mut stream, FINAL_CHUNK).await;
        })
    };
    let api = api_for(addr);
    let task = tokio::spawn(async move { api.download_attachment(CDN_URL, 1024).await });

    // Four seconds between chunks, eight seconds overall: slow, but
    // inside the one ten-second budget, so the download must succeed
    // with the exact bytes. An over-eager per-phase or shortened
    // deadline fails here.
    first_sent.notified().await;
    advance_paused(Duration::from_secs(4)).await;
    send_second.notify_one();
    second_sent.notified().await;
    advance_paused(Duration::from_secs(4)).await;
    send_final.notify_one();

    assert_eq!(
        expect_finished(task).await.unwrap().as_slice(),
        b"voice-config"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn body_at_max_bytes_succeeds_exactly() {
    let (addr, listener) = bind().await;
    let heads = recorded();
    let server = {
        let heads = heads.clone();
        tokio::spawn(async move {
            let mut stream = accept(&listener, &heads).await;
            write(
                &mut stream,
                b"HTTP/1.1 200 OK\r\ncontent-length: 16\r\n\r\n0123456789abcdef",
            )
            .await;
        })
    };
    let api = api_for(addr);
    assert_eq!(
        api.download_attachment(CDN_URL, 16)
            .await
            .unwrap()
            .as_slice(),
        b"0123456789abcdef",
        "a body of exactly max_bytes is a success"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn body_at_max_bytes_plus_one_is_refused_with_no_bytes() {
    let (addr, listener) = bind().await;
    let heads = recorded();
    let server = {
        let heads = heads.clone();
        tokio::spawn(async move {
            let mut stream = accept(&listener, &heads).await;
            write(
                &mut stream,
                b"HTTP/1.1 200 OK\r\ncontent-length: 17\r\n\r\n0123456789abcdefg",
            )
            .await;
        })
    };
    let api = api_for(addr);
    assert_eq!(
        api.download_attachment(CDN_URL, 16).await,
        Err(RoomHttpError::InvalidRequest),
        "one byte over the cap keeps today's oversized-body classification"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn non_2xx_status_is_classified_without_a_body() {
    let (addr, listener) = bind().await;
    let heads = recorded();
    let server = {
        let heads = heads.clone();
        tokio::spawn(async move {
            let mut stream = accept(&listener, &heads).await;
            write(
                &mut stream,
                b"HTTP/1.1 404 Not Found\r\ncontent-length: 2\r\n\r\n{}",
            )
            .await;
            let mut stream = accept(&listener, &heads).await;
            write(
                &mut stream,
                b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 2\r\n\r\n{}",
            )
            .await;
        })
    };
    let api = api_for(addr);
    assert_eq!(
        api.download_attachment(CDN_URL, 1024).await,
        Err(RoomHttpError::NotFound)
    );
    assert_eq!(
        api.download_attachment(CDN_URL, 1024).await,
        Err(RoomHttpError::UnknownOutcome)
    );
    server.await.unwrap();
}

#[tokio::test]
async fn redirect_is_not_followed() {
    let (addr, listener) = bind().await;
    let heads = recorded();
    let server = {
        let heads = heads.clone();
        tokio::spawn(async move {
            let mut stream = accept(&listener, &heads).await;
            write(
                &mut stream,
                b"HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1/elsewhere\r\ncontent-length: 0\r\n\r\n",
            )
            .await;
            // Give a (wrong) redirect follow a chance to land and be
            // recorded, then stop.
            let _ =
                tokio::time::timeout(Duration::from_millis(200), accept(&listener, &heads)).await;
        })
    };
    let api = api_for(addr);
    assert_eq!(
        api.download_attachment(CDN_URL, 1024).await,
        Err(RoomHttpError::Rejected {
            status: 302,
            code: 0
        }),
        "a 3xx is a classified refusal, never a followed redirect"
    );
    server.await.unwrap();
    assert_eq!(
        heads.lock().unwrap().len(),
        1,
        "no second request was made to the redirect target"
    );
}

#[tokio::test]
async fn transport_failure_mid_body_is_unknown_outcome() {
    let (addr, listener) = bind().await;
    let heads = recorded();
    let server = {
        let heads = heads.clone();
        tokio::spawn(async move {
            let mut stream = accept(&listener, &heads).await;
            // Promise 100 bytes, deliver 5, then drop the socket.
            write(
                &mut stream,
                b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nshort",
            )
            .await;
        })
    };
    let api = api_for(addr);
    assert_eq!(
        api.download_attachment(CDN_URL, 1024).await,
        Err(RoomHttpError::UnknownOutcome),
        "a truncated body is a sanitized failure with no partial bytes"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn transport_failure_before_headers_is_unknown_outcome() {
    let (addr, listener) = bind().await;
    let heads = recorded();
    let server = {
        let heads = heads.clone();
        tokio::spawn(async move {
            let _stream = accept(&listener, &heads).await;
            // Drop the accepted socket without answering.
        })
    };
    let api = api_for(addr);
    assert_eq!(
        api.download_attachment(CDN_URL, 1024).await,
        Err(RoomHttpError::UnknownOutcome)
    );
    server.await.unwrap();
}

#[tokio::test]
async fn non_cdn_or_cleartext_urls_are_refused_before_any_request() {
    let (addr, listener) = bind().await;
    let heads = recorded();
    let server = {
        let heads = heads.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_millis(200), async {
                let _ = accept(&listener, &heads).await;
            })
            .await;
        })
    };
    let api = api_for(addr);
    for url in [
        "http://cdn.discordapp.com/attachments/111/222/voice-config.json",
        "https://media.discordapp.net/attachments/111/222/voice-config.json",
        "https://cdn.discordapp.com.example.invalid/attachments/111/222/voice-config.json",
        "https://example.invalid/voice-config.json",
        "not a url",
    ] {
        assert_eq!(
            api.download_attachment(url, 1024).await,
            Err(RoomHttpError::InvalidRequest),
            "{url} must be refused by the HTTPS/CDN allowlist"
        );
    }
    server.await.unwrap();
    assert!(
        heads.lock().unwrap().is_empty(),
        "a refused URL must never reach the transport"
    );
}
