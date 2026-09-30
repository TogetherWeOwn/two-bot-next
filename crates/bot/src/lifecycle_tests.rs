//! Essential gateway termination must drain HTTP and join jobs before returning.
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::{oneshot, watch, Notify, RwLock},
};

use two_bot_core::Config;

use crate::{gateway::GatewayState, gateway_prerequisites, server, supervise_gateway};

#[test]
fn gateway_requires_all_nonempty_bindings_before_starting() {
    for token in [None, Some(""), Some("INVALID")] {
        for url in [None, Some(""), Some("synthetic-database-must-not-connect")] {
            for guild_id in [None, Some(0), Some(123)] {
                let config = Config {
                    discord_token: token.map(str::to_owned),
                    database_url: url.map(str::to_owned),
                    listen_addr: "127.0.0.1:0".into(),
                    guild_id,
                };
                let expected = if token.is_none_or(str::is_empty) {
                    Err("DISCORD_TOKEN")
                } else if url.is_none_or(str::is_empty) {
                    Err("DATABASE_URL")
                } else if guild_id.is_none_or(|id| id == 0) {
                    Err("GUILD_ID")
                } else {
                    Ok((token.unwrap(), url.unwrap(), guild_id.unwrap()))
                };
                assert_eq!(gateway_prerequisites(&config), expected);
            }
        }
    }
}

enum Termination {
    Error,
    End,
    Panic,
}

async fn stops_http(termination: Termination) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (sender, receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        receiver.await.unwrap();
        match termination {
            Termination::Error => Err(sqlx::Error::InvalidArgument("not for logs".into())),
            Termination::End => Ok(()),
            Termination::Panic => panic!("injected test task failure"),
        }
    });
    let state = Arc::new(RwLock::new(GatewayState::Armed));
    let (shutdown, stop) = watch::channel(false);
    let (cleanup_started, cleanup_observed) = oneshot::channel();
    let (finish_cleanup, cleanup_gate) = oneshot::channel();
    let http = async move {
        axum::serve(listener, server::router(state).into_make_service())
            .with_graceful_shutdown(async move {
                server::shutdown_requested(stop).await;
                cleanup_started.send(()).unwrap();
                cleanup_gate.await.unwrap();
            })
            .await
    };
    let mut service = tokio::spawn(supervise_gateway(task, http, shutdown));

    let mut socket = TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), socket.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.starts_with(b"HTTP/1.0 200"));
    sender.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), cleanup_observed)
        .await
        .expect("gateway completion must request HTTP cleanup")
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut service)
            .await
            .is_err(),
        "gateway failure must wait for gated HTTP cleanup"
    );
    finish_cleanup.send(()).unwrap();
    let error = tokio::time::timeout(Duration::from_secs(2), service)
        .await
        .unwrap()
        .unwrap()
        .expect_err("essential task termination must fail the service");
    assert_eq!(
        error.to_string(),
        "gateway task stopped; container restart required"
    );
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "health listener must stop"
    );
}

#[tokio::test]
async fn gateway_error_stops_health_service() {
    stops_http(Termination::Error).await;
}

#[tokio::test]
async fn gateway_stream_end_stops_health_service() {
    stops_http(Termination::End).await;
}

#[tokio::test]
async fn gateway_panic_stops_health_service() {
    stops_http(Termination::Panic).await;
}

struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn http_shutdown_aborts_and_joins_gateway_before_returning() {
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Dropped(dropped.clone());
    let (started, started_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _guard = guard;
        started.send(()).unwrap();
        std::future::pending::<Result<(), sqlx::Error>>().await
    });
    started_rx.await.unwrap();
    let abort = task.abort_handle();
    let (shutdown, _) = watch::channel(false);
    supervise_gateway(task, async { Ok(()) }, shutdown)
        .await
        .unwrap();
    assert!(abort.is_finished(), "gateway abort must be joined");
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn completed_gateway_requests_sticky_stop_before_first_http_poll() {
    let task = tokio::spawn(async { Ok(()) });
    while !task.is_finished() {
        tokio::task::yield_now().await;
    }
    // No receiver exists when supervise_gateway publishes cancellation.
    let (shutdown, receiver) = watch::channel(false);
    drop(receiver);
    let http_shutdown = shutdown.clone();
    let http = async move {
        assert!(*http_shutdown.borrow(), "stop must precede first HTTP poll");
        server::shutdown_requested(http_shutdown.subscribe()).await;
        Ok(())
    };
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        supervise_gateway(task, http, shutdown),
    )
    .await
    .expect("late subscriber must observe the sticky stop")
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "gateway task stopped; container restart required"
    );
}

#[tokio::test]
async fn http_server_observes_stop_before_first_poll() {
    let (shutdown, _) = watch::channel(false);
    shutdown.send_replace(true);
    tokio::time::timeout(
        Duration::from_secs(2),
        server::serve(
            "127.0.0.1:0",
            Arc::new(RwLock::new(GatewayState::Armed)),
            crate::jobs::statuses(&[], true),
            shutdown,
        ),
    )
    .await
    .expect("already stopped server must finish without an OS signal")
    .unwrap();
}

#[tokio::test]
async fn closed_stop_channel_requests_shutdown() {
    let (shutdown, receiver) = watch::channel(false);
    drop(shutdown);
    tokio::time::timeout(Duration::from_secs(2), server::shutdown_requested(receiver))
        .await
        .expect("closed stop channel must not wait for an OS signal");
}

#[tokio::test(start_paused = true)]
async fn gateway_failure_joins_pending_website_job_before_returning() {
    let dropped = Arc::new(AtomicBool::new(false));
    let starts = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let job = crate::jobs::Job {
        name: "pending",
        cadence: Duration::from_secs(1),
        startup_jitter: Duration::ZERO,
        timeout: Duration::from_secs(300),
        action: Arc::new({
            let dropped = dropped.clone();
            let starts = starts.clone();
            let started = started.clone();
            move || {
                let guard = Dropped(dropped.clone());
                let starts = starts.clone();
                let started = started.clone();
                Box::pin(async move {
                    let _guard = guard;
                    starts.fetch_add(1, Ordering::SeqCst);
                    started.notify_one();
                    std::future::pending().await
                })
            }
        }),
    };
    let status = crate::jobs::statuses(&["pending"], false);
    let (shutdown, receiver) = watch::channel(false);
    let (fail, fail_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        fail_rx.await.unwrap();
        Err(sqlx::Error::InvalidArgument("not for logs".into()))
    });
    let http =
        crate::website_jobs::serve_jobs(vec![job], status.clone(), shutdown.clone(), async move {
            server::shutdown_requested(receiver).await;
            Ok(())
        });
    let mut service = tokio::spawn(supervise_gateway(task, http, shutdown));
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("production job supervisor must start the pending action");
    assert!(status.read().await["pending"].running);
    // Gate the real supervisor's final status update after action cancellation.
    // Broadcasting alone is insufficient: the service must also join cleanup.
    let status_gate = status.write().await;
    fail.send(()).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut service)
            .await
            .is_err(),
        "service must await the job supervisor's gated cleanup"
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "pending action must be cancelled"
    );
    drop(status_gate);
    let error = tokio::time::timeout(Duration::from_secs(2), service)
        .await
        .expect("pending action must be cancelled and joined")
        .unwrap()
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "gateway task stopped; container restart required"
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "action must be dropped before return"
    );
    assert!(!status.read().await["pending"].running);
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert_eq!(starts.load(Ordering::SeqCst), 1, "no later website ticks");
}
