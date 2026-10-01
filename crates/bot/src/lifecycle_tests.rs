//! Essential gateway termination bounds HTTP/job cleanup; shutdown drains the writer.
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use futures_util::StreamExt as _;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::{oneshot, watch, Notify, RwLock},
};

use two_bot_core::Config;

use crate::{
    gateway::GatewayState, gateway_prerequisites, server, supervise_gateway,
    supervise_gateway_bounded,
};

#[test]
fn gateway_requires_all_nonempty_bindings_before_starting() {
    for token in [None, Some(""), Some("INVALID")] {
        for url in [None, Some(""), Some("synthetic-database-must-not-connect")] {
            for guild_id in [None, Some(0), Some(123)] {
                let config = Config {
                    discord_token: token.map(|value| two_bot_core::Secret::new(value.to_owned())),
                    database_url: url.map(|value| two_bot_core::Secret::new(value.to_owned())),
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
    let http_state = server::SharedState {
        gateway: Arc::clone(&state),
        database: None,
    };
    let (shutdown, stop) = watch::channel(false);
    let (cleanup_started, cleanup_observed) = oneshot::channel();
    let (finish_cleanup, cleanup_gate) = oneshot::channel();
    let http = async move {
        axum::serve(listener, server::router(http_state).into_make_service())
            .with_graceful_shutdown(async move {
                server::shutdown_requested(stop).await;
                cleanup_started.send(()).unwrap();
                cleanup_gate.await.unwrap();
            })
            .await
    };
    let mut service = tokio::spawn(supervise_gateway(task, http, Arc::clone(&state), shutdown));

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

async fn shutdown_dispatch(stalled: bool, signal_before_http_finishes: bool) {
    let state = Arc::new(RwLock::new(GatewayState::Connected));
    let stop_state = Arc::clone(&state);
    let (shutdown, mut stopping) = watch::channel(false);
    let (release, wait) = std::sync::mpsc::channel();
    let (started, start) = oneshot::channel();
    let (queued, accepted) = oneshot::channel();
    let (stopped, stop) = oneshot::channel();
    let rows = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = Arc::clone(&rows);
    let stream = futures_util::stream::iter([0, 1])
        .chain(futures_util::stream::once(async move {
            queued.send(()).unwrap();
            std::future::pending::<i32>().await
        }))
        .take_until(async move {
            stopping.wait_for(|stopping| *stopping).await.unwrap();
        });
    let mut started = Some(started);
    let task = tokio::spawn(async move {
        crate::dispatch::dispatch_bounded(
            stream,
            8,
            move |n| {
                if n == 0 {
                    started.take().unwrap().send(()).unwrap();
                    // No handler-side timer: fatal supervision must finish
                    // before this fixture releases a permanently stalled writer.
                    wait.recv().unwrap();
                }
                observed.lock().unwrap().push(n);
            },
            || async move {
                *stop_state.write().await = GatewayState::Draining;
                stopped.send(()).unwrap();
            },
            if stalled {
                Duration::from_millis(80)
            } else {
                Duration::from_secs(2)
            },
            Duration::from_secs(2),
        )
        .await
        .map_err(|reason| sqlx::Error::InvalidArgument(reason.into()))
    });
    start.await.unwrap();
    accepted.await.unwrap();
    let mut service = tokio::spawn(supervise_gateway_bounded(
        task,
        async move {
            if signal_before_http_finishes {
                std::future::pending().await
            } else {
                Ok(())
            }
        },
        Arc::clone(&state),
        shutdown.clone(),
        Duration::from_secs(1),
    ));
    if signal_before_http_finishes {
        shutdown.send_replace(true);
    }
    tokio::time::timeout(Duration::from_secs(1), stop)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*state.read().await, GatewayState::Draining);
    assert!(
        !service.is_finished(),
        "shutdown must not detach the writer"
    );
    if !stalled {
        release.send(()).unwrap();
    }
    let result = tokio::time::timeout(Duration::from_secs(2), &mut service).await;
    if stalled {
        // Always release before asserting so failures cannot hang test shutdown.
        release.send(()).unwrap();
    }
    let result = result.unwrap().unwrap();
    if stalled {
        assert!(result.is_err(), "deadline must force the process-exit path");
    } else {
        result.unwrap();
        assert_eq!(*rows.lock().unwrap(), vec![0, 1]);
    }
    assert_eq!(*state.read().await, GatewayState::Draining);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_completion_waits_for_accepted_dispatches_and_keeps_unready() {
    shutdown_dispatch(false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_shutdown_retains_stalled_writer_deadline() {
    shutdown_dispatch(true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_signal_bounds_writer_even_while_http_is_still_draining() {
    shutdown_dispatch(true, true).await;
}

#[tokio::test]
async fn shutdown_bounds_a_gateway_that_never_observes_stop() {
    let task = tokio::spawn(std::future::pending::<Result<(), sqlx::Error>>());
    let abort = task.abort_handle();
    let state = Arc::new(RwLock::new(GatewayState::Connected));
    let result = supervise_gateway_bounded(
        task,
        async { Ok(()) },
        Arc::clone(&state),
        watch::channel(false).0,
        Duration::from_millis(20),
    )
    .await;
    abort.abort(); // Fixture cleanup only; production exits on the returned Err.
    assert_eq!(
        result.unwrap_err().to_string(),
        "service shutdown deadline exceeded"
    );
    assert_eq!(*state.read().await, GatewayState::Draining);
}

struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn shutdown_retains_gateway_handle_through_gated_http_cleanup() {
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Dropped(dropped.clone());
    let state = Arc::new(RwLock::new(GatewayState::Connected));
    let (shutdown, stop) = watch::channel(false);
    let http_stop = shutdown.subscribe();
    let (started, started_rx) = oneshot::channel();
    let (finish_gateway, gateway_gate) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _guard = guard;
        started.send(()).unwrap();
        server::shutdown_requested(stop).await;
        gateway_gate.await.unwrap();
        Ok(())
    });
    started_rx.await.unwrap();
    let completion = task.abort_handle();
    let (cleanup_started, cleanup_observed) = oneshot::channel();
    let (finish_http, http_gate) = oneshot::channel();
    let http = async move {
        server::shutdown_requested(http_stop).await;
        cleanup_started.send(()).unwrap();
        http_gate.await.unwrap();
        Ok(())
    };
    let mut service = tokio::spawn(supervise_gateway(
        task,
        http,
        Arc::clone(&state),
        shutdown.clone(),
    ));
    shutdown.send_replace(true);
    tokio::time::timeout(Duration::from_secs(2), cleanup_observed)
        .await
        .expect("HTTP cleanup must be polled while the writer drains")
        .unwrap();
    assert_eq!(*state.read().await, GatewayState::Draining);
    assert!(!completion.is_finished(), "gateway must not be aborted");
    assert!(!dropped.load(Ordering::SeqCst));
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut service)
            .await
            .is_err()
    );
    finish_http.send(()).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut service)
            .await
            .is_err(),
        "HTTP completion must still retain the gateway handle"
    );
    finish_gateway.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), service)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(completion.is_finished(), "gateway drain must be joined");
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(*state.read().await, GatewayState::Draining);
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
        supervise_gateway(
            task,
            http,
            Arc::new(RwLock::new(GatewayState::Armed)),
            shutdown,
        ),
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
            server::bind("127.0.0.1:0").await.unwrap(),
            server::SharedState {
                gateway: Arc::new(RwLock::new(GatewayState::Armed)),
                database: None,
            },
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
async fn stopped_http_drain_has_a_deadline() {
    let (shutdown, _) = watch::channel(false);
    let (started, started_rx) = oneshot::channel();
    let http = async move {
        started.send(()).unwrap();
        std::future::pending().await
    };
    let service = tokio::spawn(crate::website_jobs::serve_jobs(
        vec![],
        crate::jobs::statuses(&[], true),
        shutdown.clone(),
        http,
    ));
    started_rx.await.unwrap();
    shutdown.send_replace(true);
    let stopped_at = tokio::time::Instant::now();
    let error = tokio::time::timeout(
        crate::website_jobs::HTTP_DRAIN_TIMEOUT + Duration::from_secs(1),
        service,
    )
    .await
    .expect("HTTP draining must not wait forever")
    .unwrap()
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(error.to_string(), "HTTP shutdown drain timed out");
    assert!(stopped_at.elapsed() >= crate::website_jobs::HTTP_DRAIN_TIMEOUT);
}

#[tokio::test(start_paused = true)]
async fn stalled_http_drain_does_not_bypass_website_job_join() {
    let dropped = Arc::new(AtomicBool::new(false));
    let http_dropped = Arc::new(AtomicBool::new(false));
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
    let (shutdown, _) = watch::channel(false);
    let (fail, fail_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        fail_rx.await.unwrap();
        Err(sqlx::Error::InvalidArgument("not for logs".into()))
    });
    let http_guard = Dropped(http_dropped.clone());
    let http =
        crate::website_jobs::serve_jobs(vec![job], status.clone(), shutdown.clone(), async move {
            let _guard = http_guard;
            std::future::pending().await
        });
    let mut service = tokio::spawn(supervise_gateway(
        task,
        http,
        Arc::new(RwLock::new(GatewayState::Armed)),
        shutdown,
    ));
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("production job supervisor must start the pending action");
    // The deadline is for draining after stop, not the HTTP service's lifetime.
    tokio::time::advance(crate::website_jobs::HTTP_DRAIN_TIMEOUT * 2).await;
    tokio::task::yield_now().await;
    assert!(!http_dropped.load(Ordering::SeqCst));
    assert!(!service.is_finished());
    let status_gate = status.write().await;
    fail.send(()).unwrap();
    assert!(
        tokio::time::timeout(
            crate::website_jobs::HTTP_DRAIN_TIMEOUT + Duration::from_millis(25),
            &mut service,
        )
        .await
        .is_err(),
        "HTTP deadline must not bypass the job supervisor's gated cleanup"
    );
    assert!(http_dropped.load(Ordering::SeqCst), "HTTP drain is bounded");
    assert!(
        dropped.load(Ordering::SeqCst),
        "job action must be cancelled"
    );
    drop(status_gate);
    let error = tokio::time::timeout(Duration::from_secs(1), service)
        .await
        .expect("restart must finish once independent job cleanup completes")
        .unwrap()
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "gateway task stopped; container restart required"
    );
    assert!(!status.read().await["pending"].running);
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert_eq!(starts.load(Ordering::SeqCst), 1, "no later website ticks");
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
    let mut service = tokio::spawn(supervise_gateway(
        task,
        http,
        Arc::new(RwLock::new(GatewayState::Armed)),
        shutdown,
    ));
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
