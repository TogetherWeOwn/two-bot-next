//! Essential gateway termination cancels HTTP; HTTP shutdown drains the writer.
use std::{sync::Arc, time::Duration};

use futures_util::StreamExt as _;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::{oneshot, watch, RwLock},
};

use crate::{gateway::GatewayState, server, supervise_gateway, supervise_gateway_bounded};

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
    let http_state = Arc::clone(&state);
    let service = tokio::spawn(supervise_gateway(
        task,
        async move {
            axum::serve(
                listener,
                server::router(server::SharedState {
                    gateway: http_state,
                    database: None,
                })
                .into_make_service(),
            )
            .await
        },
        state,
        watch::channel(false).0,
    ));

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
