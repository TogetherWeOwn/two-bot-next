//! Durable admission that becomes ready exactly at the caller's deadline.
//!
//! `tokio::time::timeout_at` polls its inner future before its timer, so an
//! admission that resolves while the clock sits at the deadline is returned as
//! `Ok(permit)`. The transport must recheck the deadline after admission,
//! before any Hyper request, and release the provably unused permit. Dropping
//! it instead would leave the durable lane occupied for the whole lease.
//!
//! The clock is paused. The synthetic admission applies the advance by polling
//! a pinned `tokio::time::advance` future exactly once (its first poll moves
//! the frozen clock base synchronously, then yields), so time reaches the
//! deadline without ever returning control to the runtime.

#![cfg(test)]

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;
use two_bot_core::send_admission::{
    AdmissionError, AdmissionFuture, AdmissionPermit, SendAdmission, SendCompletion, SendCooldown,
    TokenKey,
};

const TOKEN: &str = "fixture-deadline-equality-token";
const CHANNEL: &str = "111111111111111111";
const MESSAGE_ID: &str = "222222222222222222";

/// Records every completion so release and uncertain drop are distinguishable.
#[derive(Default)]
struct Ledger {
    admits: AtomicUsize,
    completions: AtomicUsize,
    cooldowns: StdMutex<Vec<Option<SendCooldown>>>,
}

struct CountingCompletion(Arc<Ledger>);

impl SendCompletion for CountingCompletion {
    fn complete(
        self: Box<Self>,
        cooldown: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        Box::pin(async move {
            self.0.completions.fetch_add(1, Ordering::SeqCst);
            self.0.cooldowns.lock().unwrap().push(cooldown);
            Ok(())
        })
    }
}

#[derive(Debug)]
struct SyntheticAdmission {
    key: TokenKey,
    ledger: Arc<Ledger>,
    /// Paused-clock time to apply inside the admission's own poll.
    advance_by: Option<Duration>,
}

impl SendAdmission for SyntheticAdmission {
    fn token_key(&self) -> &TokenKey {
        &self.key
    }

    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        Box::pin(async move {
            self.ledger.admits.fetch_add(1, Ordering::SeqCst);
            if let Some(by) = self.advance_by {
                // One poll applies the clock advance; its trailing yield is
                // deliberately not awaited, so this admission stays Ready in
                // the same poll that `timeout_at` gives its inner future.
                let mut advance = Box::pin(tokio::time::advance(by));
                std::future::poll_fn(|cx| {
                    assert!(std::future::Future::poll(advance.as_mut(), cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
            }
            Ok(AdmissionPermit::new(Box::new(CountingCompletion(
                self.ledger.clone(),
            ))))
        })
    }
}

impl std::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ledger").finish_non_exhaustive()
    }
}

fn executor(origin: String, advance_by: Option<Duration>) -> (ActionExecutor, Arc<Ledger>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ledger = Arc::new(Ledger::default());
    let executor = ActionExecutor::with_admission(
        TOKEN.to_owned(),
        Some(origin),
        Arc::new(SyntheticAdmission {
            key: TokenKey::for_bot_token(TOKEN).expect("fixture token key"),
            ledger: ledger.clone(),
            advance_by,
        }),
    )
    .expect("fixture executor");
    (executor, ledger)
}

/// Serve exactly one request on loopback and record its head and body.
fn serve_one(listener: tokio::net::TcpListener) -> tokio::task::JoinHandle<(String, Vec<u8>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut raw = Vec::new();
        let mut byte = [0u8; 1];
        while !raw.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).await.expect("read head");
            raw.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&raw).into_owned();
        let length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).await.expect("read body");
        let payload = format!(r#"{{"id":"{MESSAGE_ID}"}}"#);
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
            payload.len()
        );
        stream.write_all(response.as_bytes()).await.expect("write");
        stream.flush().await.expect("flush");
        (head, body)
    })
}

/// Admission ready at `now == deadline` must release its permit and send nothing.
#[tokio::test(start_paused = true)]
async fn admission_ready_at_deadline_releases_permit_before_any_request() {
    // Never accepted by design: any connect attempt would still land in the
    // backlog, so a successful non-blocking accept proves a request escaped.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let origin = format!("http://{}", listener.local_addr().expect("addr"));
    let budget = Duration::from_millis(750);
    let (executor, ledger) = executor(origin, Some(budget));

    let deadline = tokio::time::Instant::now() + budget;
    let nonce = format!("deadline-{}", listener.local_addr().expect("addr").port());
    let result = executor
        .post_message_confirmed_before(CHANNEL, "reminder", &nonce, deadline)
        .await;

    assert_eq!(
        tokio::time::Instant::now(),
        deadline,
        "fixture must leave the clock exactly at the deadline"
    );
    assert_eq!(result, Err(DiscordError::Timeout));
    assert_eq!(ledger.admits.load(Ordering::SeqCst), 1);
    assert_eq!(
        ledger.completions.load(Ordering::SeqCst),
        1,
        "a provably unused permit must be released, not dropped uncertain"
    );
    assert_eq!(*ledger.cooldowns.lock().unwrap(), vec![None]);
    assert!(
        matches!(
            listener.accept(),
            Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock
        ),
        "no connection or request may reach the loopback server"
    );
}

/// Live control: the same fixture with deadline room sends once and confirms.
#[tokio::test]
async fn admission_ready_before_deadline_sends_and_confirms() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let nonce = format!("deadline-{}", listener.local_addr().expect("addr").port());
    let origin = format!("http://{}", listener.local_addr().expect("addr"));
    let server = serve_one(listener);
    let (executor, ledger) = executor(origin, None);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let id = tokio::time::timeout(
        Duration::from_secs(15),
        executor.post_message_confirmed_before(CHANNEL, "reminder", &nonce, deadline),
    )
    .await
    .expect("control send finished")
    .expect("control send succeeds");
    assert_eq!(id, MESSAGE_ID);

    let (head, body) = server.await.expect("server task");
    assert!(
        head.starts_with("POST ") && head.contains(&format!("/channels/{CHANNEL}/messages")),
        "unexpected request head: {head}"
    );
    let body: serde_json::Value = serde_json::from_slice(&body).expect("json body");
    assert_eq!(body["content"], "reminder");
    assert_eq!(body["nonce"], nonce);
    assert_eq!(ledger.admits.load(Ordering::SeqCst), 1);
    assert_eq!(
        ledger.completions.load(Ordering::SeqCst),
        1,
        "confirmed receipt completes the permit exactly once"
    );
}
