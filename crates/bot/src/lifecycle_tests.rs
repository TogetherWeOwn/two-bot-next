//! Essential gateway termination must cancel HTTP, not leave healthy zombies.
use std::{sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::{oneshot, RwLock},
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
    let service = tokio::spawn(supervise_gateway(task, async move {
        axum::serve(listener, server::router(state).into_make_service()).await
    }));

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

#[tokio::test]
async fn http_shutdown_aborts_gateway_task() {
    let task = tokio::spawn(std::future::pending::<Result<(), sqlx::Error>>());
    let abort = task.abort_handle();
    supervise_gateway(task, async { Ok(()) }).await.unwrap();
    tokio::task::yield_now().await;
    assert!(abort.is_finished());
}
