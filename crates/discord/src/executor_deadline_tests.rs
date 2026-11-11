//! Synthetic admission for the transport's absolute deadline boundary.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use two_bot_core::send_admission::{
    AdmissionError, AdmissionFuture, AdmissionPermit, SendAdmission, SendCompletion, SendCooldown,
    TokenKey,
};

const TOKEN: &str = "fixture-deadline-token";

#[derive(Debug, Default)]
struct Ledger {
    admits: AtomicUsize,
    completions: AtomicUsize,
    started: AtomicUsize,
}

#[derive(Clone, Copy, Debug)]
enum Settlement {
    Immediate,
    Yielding,
    Stuck,
}

struct Completion(Arc<Ledger>, Settlement);

impl SendCompletion for Completion {
    fn complete(
        self: Box<Self>,
        _: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        Box::pin(async move {
            self.0.started.fetch_add(1, Ordering::SeqCst);
            match self.1 {
                Settlement::Immediate => {}
                Settlement::Yielding => tokio::task::yield_now().await,
                Settlement::Stuck => std::future::pending::<()>().await,
            }
            self.0.completions.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

#[derive(Debug)]
struct Admission {
    key: TokenKey,
    ledger: Arc<Ledger>,
    advance: Duration,
    settlement: Settlement,
}

impl SendAdmission for Admission {
    fn token_key(&self) -> &TokenKey {
        &self.key
    }

    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        Box::pin(async move {
            self.ledger.admits.fetch_add(1, Ordering::SeqCst);
            // Apply the frozen-clock advance in the admission poll, without
            // yielding back to the timeout before returning the permit.
            if !self.advance.is_zero() {
                let mut advance = Box::pin(tokio::time::advance(self.advance));
                std::future::poll_fn(|cx| {
                    assert!(std::future::Future::poll(advance.as_mut(), cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
            }
            Ok(AdmissionPermit::new(Box::new(Completion(
                self.ledger.clone(),
                self.settlement,
            ))))
        })
    }
}

fn fixture(
    advance: Duration,
    settlement: Settlement,
) -> (HyperTransport, Arc<Ledger>, std::net::TcpListener) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let ledger = Arc::new(Ledger::default());
    let transport = HyperTransport::with_admission(
        TOKEN.to_owned(),
        Some(format!(
            "http://{}",
            listener.local_addr().expect("address")
        )),
        Arc::new(Admission {
            key: TokenKey::for_bot_token(TOKEN).expect("token key"),
            ledger: ledger.clone(),
            advance,
            settlement,
        }),
    )
    .expect("transport");
    (transport, ledger, listener)
}

fn request() -> Request {
    Request::builder(&Route::CreateMessage { channel_id: 1 })
        .build()
        .expect("request")
}

#[tokio::test(start_paused = true)]
async fn equality_releases_unused_permit_without_wire() {
    let budget = Duration::from_millis(750);
    let (transport, ledger, listener) = fixture(budget, Settlement::Immediate);
    let deadline = tokio::time::Instant::now() + budget;
    let result = transport.send_request(&request(), Some(deadline)).await;
    assert!(matches!(result, Err(ref error) if error == HEADER_TIMEOUT_MESSAGE));
    assert_eq!(tokio::time::Instant::now(), deadline);
    assert_eq!(ledger.admits.load(Ordering::SeqCst), 1);
    assert_eq!(ledger.completions.load(Ordering::SeqCst), 1);
    assert!(
        matches!(listener.accept(), Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}

#[tokio::test(start_paused = true)]
async fn expired_permit_settlement_yields_or_times_out_in_place() {
    for (mode, completed) in [(Settlement::Yielding, 1), (Settlement::Stuck, 0)] {
        let budget = Duration::from_millis(750);
        let (transport, ledger, listener) = fixture(budget, mode);
        let deadline = tokio::time::Instant::now() + budget;
        let result = transport.send_request(&request(), Some(deadline)).await;
        assert!(matches!(result, Err(ref error) if error == HEADER_TIMEOUT_MESSAGE));
        assert_eq!(ledger.started.load(Ordering::SeqCst), 1);
        assert_eq!(ledger.completions.load(Ordering::SeqCst), completed);
        assert!(tokio::time::Instant::now() <= deadline + Duration::from_millis(500));
        assert!(
            matches!(listener.accept(), Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_settlement_does_not_complete_permit() {
    let budget = Duration::from_millis(750);
    let (transport, ledger, listener) = fixture(budget, Settlement::Stuck);
    let deadline = tokio::time::Instant::now() + budget;
    let request = request();
    let mut send = Box::pin(transport.send_request(&request, Some(deadline)));
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(send.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(ledger.started.load(Ordering::SeqCst), 1);
    drop(send);
    assert_eq!(ledger.completions.load(Ordering::SeqCst), 0);
    assert!(
        matches!(listener.accept(), Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}

#[tokio::test(start_paused = true)]
async fn expired_deadline_never_admits_or_sends() {
    let (transport, ledger, listener) = fixture(Duration::ZERO, Settlement::Immediate);
    let deadline = tokio::time::Instant::now();
    let result = transport.send_request(&request(), Some(deadline)).await;
    assert!(matches!(result, Err(ref error) if error == HEADER_TIMEOUT_MESSAGE));
    assert_eq!(ledger.admits.load(Ordering::SeqCst), 0);
    assert_eq!(ledger.completions.load(Ordering::SeqCst), 0);
    assert!(
        matches!(listener.accept(), Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}

#[tokio::test(start_paused = true)]
async fn status_only_and_cutover_delete_release_at_equality() {
    let budget = Duration::from_millis(750);
    for status_only in [true, false] {
        let (transport, ledger, listener) = fixture(budget, Settlement::Yielding);
        let deadline = tokio::time::Instant::now() + budget;
        let delete = TwilightClient::new(TOKEN.to_owned())
            .delete_channel(twilight_model::id::Id::new(1))
            .try_into_request()
            .expect("delete request");
        // Exercise the same two transport seams used by status-only and cutover.
        let result = if status_only {
            transport
                .send_request_headers(&delete, Some(deadline))
                .await
                .err()
        } else {
            transport.send_request(&delete, Some(deadline)).await.err()
        };
        assert_eq!(result.as_deref(), Some(HEADER_TIMEOUT_MESSAGE));
        assert_eq!(ledger.completions.load(Ordering::SeqCst), 1);
        assert!(
            matches!(listener.accept(), Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn local_admission_at_deadline_proves_no_effect() {
    let budget = Duration::from_millis(750);
    let (transport, ledger, listener) = fixture(Duration::ZERO, Settlement::Immediate);
    let mut executor =
        ActionExecutor::assemble_with_timeout(TOKEN.to_owned(), None, transport, budget)
            .expect("executor");
    let (probe, mut admissions) = tokio::sync::mpsc::unbounded_channel();
    Arc::get_mut(&mut executor.inner).unwrap().pacing_probe = Some(probe);
    let lane = executor.inner.pace_last_at.lock().await;
    let request = request();
    let mut send = Box::pin(executor.send_with_timeout_for(&request, Some(false), budget));
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(send.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    tokio::time::advance(budget).await;
    drop(lane);
    let error = send.await.expect_err("deadline must refuse the send");
    assert_eq!(
        admissions.try_recv().expect("local admission completed").at,
        tokio::time::Instant::now()
    );
    assert!(matches!(
        error,
        DiscordError::Guard(GuardError::AdmissionTimeout)
    ));
    assert!(error.proves_no_effect());
    assert_eq!(ledger.admits.load(Ordering::SeqCst), 0);
    assert!(
        matches!(listener.accept(), Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}

#[tokio::test(start_paused = true)]
async fn status_entry_point_releases_at_deadline_equality() {
    let budget = Duration::from_millis(750);
    let (transport, ledger, listener) = fixture(budget, Settlement::Yielding);
    let executor = ActionExecutor::assemble_with_timeout(
        TOKEN.to_owned(),
        Some(format!(
            "http://{}",
            listener.local_addr().expect("address")
        )),
        transport,
        budget,
    )
    .expect("executor");
    let result = executor.send_status(&request()).await;
    assert!(matches!(result, Err(ref error) if error == HEADER_TIMEOUT_MESSAGE));
    assert_eq!(ledger.admits.load(Ordering::SeqCst), 1);
    assert_eq!(ledger.completions.load(Ordering::SeqCst), 1);
    assert!(
        matches!(listener.accept(), Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}

#[tokio::test]
async fn successful_wire_preserves_mutation_receipt() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (transport, ledger, listener) = fixture(Duration::ZERO, Settlement::Immediate);
    let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut bytes = [0; 4096];
        assert!(socket.read(&mut bytes).await.expect("request") > 0);
        socket
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
            .await
            .expect("response");
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let (response, _) = transport
        .send_request(&request(), Some(deadline))
        .await
        .expect("response");
    assert_eq!(response.status, 204);
    assert!(response.completion.is_some());
    assert_eq!(ledger.admits.load(Ordering::SeqCst), 1);
    assert_eq!(ledger.completions.load(Ordering::SeqCst), 0);
    server.await.expect("server");
}

#[tokio::test]
async fn cutover_delete_wire_timeout_keeps_uncertain_permit() {
    use tokio::io::AsyncReadExt;
    let (transport, ledger, listener) = fixture(Duration::ZERO, Settlement::Immediate);
    let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
    let (received, seen) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut bytes = [0; 4096];
        assert!(socket.read(&mut bytes).await.expect("request") > 0);
        received.send(()).expect("receiver");
        std::future::pending::<()>().await;
    });
    let delete = TwilightClient::new(TOKEN.to_owned())
        .delete_channel(twilight_model::id::Id::new(1))
        .try_into_request()
        .expect("delete request");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let send = transport.send_request(&delete, Some(deadline));
    let (result, observed) = tokio::join!(send, seen);
    observed.expect("wire observed");
    assert!(matches!(result, Err(ref error) if error == HEADER_TIMEOUT_MESSAGE));
    assert_eq!(ledger.admits.load(Ordering::SeqCst), 1);
    assert_eq!(ledger.completions.load(Ordering::SeqCst), 0);
    server.abort();
}
