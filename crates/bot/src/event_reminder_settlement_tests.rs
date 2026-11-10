//! Clock conversion and pre-wire settlement through the production reminder path.

#![cfg(test)]

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use two_bot_core::send_admission::{
    AdmissionError, AdmissionFuture, AdmissionPermit, SendAdmission, SendCompletion, SendCooldown,
    TokenKey,
};

const TOKEN: &str = "fixture-reminder-settlement";
const CHANNEL: &str = "111111111111111111";

#[derive(Debug, Default)]
struct Lane {
    occupied: AtomicBool,
    admits: AtomicUsize,
    completions: AtomicUsize,
}

struct Completion {
    lane: Arc<Lane>,
    delay: Duration,
}

impl SendCompletion for Completion {
    fn complete(
        self: Box<Self>,
        cooldown: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        Box::pin(async move {
            assert_eq!(cooldown, None);
            // Like the production pool acquire/UPDATE, completion must yield
            // before releasing occupancy. Dropping this future keeps it held.
            tokio::time::sleep(self.delay).await;
            self.lane.completions.fetch_add(1, Ordering::SeqCst);
            self.lane.occupied.store(false, Ordering::SeqCst);
            Ok(())
        })
    }
}

#[derive(Debug)]
struct Admission {
    key: TokenKey,
    lane: Arc<Lane>,
    advance_by: Duration,
    completion_delay: Duration,
}

/// Move paused time within one poll, modeling suspension without an await
/// between the clock sample/admission and the next deadline check.
async fn advance_in_poll(by: Duration) {
    let mut advance = Box::pin(tokio::time::advance(by));
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(advance.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
}

impl SendAdmission for Admission {
    fn token_key(&self) -> &TokenKey {
        &self.key
    }

    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        Box::pin(async move {
            if self.lane.occupied.swap(true, Ordering::SeqCst) {
                return Err(AdmissionError::Blocked);
            }
            self.lane.admits.fetch_add(1, Ordering::SeqCst);
            if !self.advance_by.is_zero() {
                advance_in_poll(self.advance_by).await;
            }
            Ok(AdmissionPermit::new(Box::new(Completion {
                lane: self.lane.clone(),
                delay: self.completion_delay,
            })))
        })
    }
}

fn fixture(
    advance_by: Duration,
    completion_delay: Duration,
) -> (ActionExecutor, Arc<Admission>, std::net::TcpListener) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let admission = Arc::new(Admission {
        key: TokenKey::for_bot_token(TOKEN).expect("fixture key"),
        lane: Arc::new(Lane::default()),
        advance_by,
        completion_delay,
    });
    let rest = ActionExecutor::with_admission(
        TOKEN.into(),
        Some(format!("http://{}", listener.local_addr().expect("addr"))),
        admission.clone(),
    )
    .expect("fixture executor");
    (rest, admission, listener)
}

fn assert_no_http(listener: &std::net::TcpListener) {
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "even a connection must not start after the wire deadline"
    );
}

#[tokio::test(start_paused = true)]
async fn suspension_after_wall_sample_does_not_extend_the_wire_deadline() {
    let epoch = tokio::time::Instant::now();
    let (rest, admission, listener) = fixture(Duration::ZERO, Duration::from_millis(10));
    let wall = 1_800_000_000_000;
    let (_, deadline) = event_reminder_deadline(&format_iso_ms(wall + 100), &|| {
        // The sampled wall time has 100 ms left, but the thread resumes 200 ms
        // later. Apply that advance synchronously inside this clock sample.
        let mut advance = Box::pin(tokio::time::advance(Duration::from_millis(200)));
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        assert!(std::future::Future::poll(advance.as_mut(), &mut cx).is_pending());
        wall
    });
    let deadline = deadline.expect("sample was before start");
    assert_eq!(deadline, epoch + Duration::from_millis(100));
    assert!(tokio::time::Instant::now() > deadline);
    assert_eq!(
        send_event_reminder(&rest, CHANNEL, "reminder", deadline).await,
        Err(DiscordError::Timeout)
    );
    assert_eq!(admission.lane.admits.load(Ordering::SeqCst), 0);
    assert_no_http(&listener);
}

#[tokio::test(start_paused = true)]
async fn expired_reminder_settles_yielding_permit_and_allows_next_admission() {
    let budget = Duration::from_millis(100);
    let (rest, admission, listener) = fixture(budget, Duration::from_millis(10));
    let epoch = tokio::time::Instant::now();
    assert_eq!(
        send_event_reminder(&rest, CHANNEL, "reminder", epoch + budget).await,
        Err(DiscordError::Timeout)
    );
    assert!(tokio::time::Instant::now() > epoch + budget);
    assert_eq!(admission.lane.completions.load(Ordering::SeqCst), 1);
    assert_no_http(&listener);
    let next = admission.admit().await.expect("unused permit was released");
    assert_eq!(admission.lane.admits.load(Ordering::SeqCst), 2);
    next.complete(None).await.expect("fixture cleanup");
}

#[tokio::test(start_paused = true)]
async fn urgent_watch_reserves_settlement_before_its_cancellation_boundary() {
    let epoch = tokio::time::Instant::now();
    let watch_until = epoch + Duration::from_secs(1);
    let wire_until = reminder_watch_wire_deadline(watch_until);
    let (rest, admission, listener) = fixture(wire_until - epoch, Duration::from_millis(10));
    watch_urgent_reminders(watch_until, |_| async {
        assert_eq!(
            send_event_reminder(&rest, CHANNEL, "reminder", wire_until).await,
            Err(DiscordError::Timeout)
        );
        Ok((1, false))
    })
    .await
    .expect("watch settled");
    assert!(tokio::time::Instant::now() < watch_until);
    assert_eq!(admission.lane.completions.load(Ordering::SeqCst), 1);
    assert_no_http(&listener);
    admission
        .admit()
        .await
        .expect("watch did not strand lane")
        .complete(None)
        .await
        .expect("fixture cleanup");
}

#[tokio::test(start_paused = true)]
async fn stuck_safe_release_is_bounded_and_does_not_falsely_free_the_lane() {
    let budget = Duration::from_millis(100);
    let (rest, admission, listener) = fixture(budget, Duration::from_secs(60));
    let epoch = tokio::time::Instant::now();
    assert_eq!(
        send_event_reminder(&rest, CHANNEL, "reminder", epoch + budget).await,
        Err(DiscordError::Timeout)
    );
    assert_eq!(
        tokio::time::Instant::now(),
        epoch + budget + reminder_settlement_grace()
    );
    assert_eq!(admission.lane.completions.load(Ordering::SeqCst), 0);
    assert!(matches!(
        admission.admit().await,
        Err(AdmissionError::Blocked)
    ));
    assert_no_http(&listener);
    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(
        admission.lane.completions.load(Ordering::SeqCst),
        0,
        "no detached cleanup"
    );
}

#[tokio::test]
async fn wire_timeout_keeps_the_uncertain_mutation_permit_occupied() {
    use tokio::io::AsyncReadExt;

    let (rest, admission, listener) = fixture(Duration::ZERO, Duration::from_millis(10));
    let listener = tokio::net::TcpListener::from_std(listener).expect("async listener");
    let (received, request) = tokio::sync::oneshot::channel();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut head = Vec::new();
        loop {
            head.push(stream.read_u8().await.expect("request head"));
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        assert!(head.starts_with(b"POST "));
        received.send(()).expect("request observed");
        // HTTP started, but no headers or receipt are returned.
        stopped.await.expect("stop server");
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let result = send_event_reminder(&rest, CHANNEL, "reminder", deadline).await;
    tokio::time::timeout(Duration::from_secs(1), request)
        .await
        .expect("HTTP observed")
        .expect("request signal");
    assert_eq!(result, Err(DiscordError::Timeout));
    assert_eq!(admission.lane.completions.load(Ordering::SeqCst), 0);
    assert!(matches!(
        admission.admit().await,
        Err(AdmissionError::Blocked)
    ));
    stop.send(()).expect("stop");
    server.await.expect("server joined");
}
