//! Loopback acceptance: probe and echo against each other on 127.0.0.1.
//!
//! - Discovery reports the probe's egress address as the echo sees it.
//! - A short RTT run over loopback loses nothing.
//! - Without an echo, discovery fails cleanly and the run still completes
//!   with loss numbers (exit 0 in the binary, `Ok` here).
//! - Duplicates and stray packets never inflate the receipt count.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::time::Duration;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::net::UdpSocket;
use two_bot_voice_probe::discovery::{decode_response, encode_response, is_discovery_request};
use two_bot_voice_probe::runner::{decode_rtt, run_echo_on, run_probe, EchoStats, ProbeConfig};

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

fn short_probe(echo: SocketAddr) -> ProbeConfig {
    ProbeConfig {
        drain: Duration::from_millis(200),
        discovery_attempts: 2,
        discovery_timeout: Duration::from_millis(200),
        duration: Duration::from_secs(1),
        rate_per_sec: 10,
        ..ProbeConfig::new(echo.to_string())
    }
}

async fn spawn_echo() -> (
    SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<EchoStats>,
) {
    let socket = UdpSocket::bind(loopback(0)).await.unwrap();
    let addr = socket.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        run_echo_on(socket, async move {
            shutdown_rx.await.ok();
        })
        .await
        .unwrap()
    });
    (addr, shutdown_tx, handle)
}

#[tokio::test]
async fn loopback_discovery_reports_egress() {
    let (addr, shutdown, echo) = spawn_echo().await;

    let evidence = run_probe(&short_probe(addr)).await.unwrap();

    shutdown.send(()).ok();
    let stats = echo.await.unwrap();
    assert!(evidence.discovery_ok);
    let egress = evidence.observed_egress.unwrap();
    assert!(egress.starts_with("127.0.0.1:"), "egress was {egress}");
    assert!(stats.discovery_replies >= 1);
}

#[tokio::test]
async fn loopback_short_run_has_zero_loss() {
    let (addr, shutdown, echo) = spawn_echo().await;
    let mut config = short_probe(addr);
    config.duration = Duration::from_secs(2);
    config.rate_per_sec = 20;

    let evidence = run_probe(&config).await.unwrap();

    shutdown.send(()).ok();
    echo.await.unwrap();
    assert!(evidence.sent > 0);
    assert_eq!(evidence.received, evidence.sent);
    assert_eq!(evidence.loss_pct, 0.0);
    assert!(evidence.rtt_ms.min.unwrap() >= 0.0);
    assert!(evidence.rtt_ms.max.unwrap() >= evidence.rtt_ms.min.unwrap());
    assert!(evidence.duration_s >= 2.0);
    assert!(evidence.duration_s < 5.0);
    assert_eq!(evidence.echo_host, addr.to_string());
    assert!(OffsetDateTime::parse(&evidence.started_at, &Rfc3339).is_ok());
}

#[tokio::test]
async fn missing_echo_fails_discovery_but_completes_the_run() {
    // Silent target: bound and held for the whole probe so no other test can
    // grab the port, but never read — every packet times out. Releasing the
    // port instead races parallel tests: a reused port delivers this probe's
    // discovery requests to another test's socket.
    let held = StdUdpSocket::bind(loopback(0)).unwrap();
    let config = short_probe(held.local_addr().unwrap());

    let evidence = run_probe(&config).await.unwrap();

    assert!(!evidence.discovery_ok);
    assert_eq!(evidence.observed_egress, None);
    assert!(evidence.sent > 0);
    assert_eq!(evidence.received, 0);
    assert_eq!(evidence.loss_pct, 100.0);
    assert_eq!(evidence.rtt_ms.min, None);
}

#[tokio::test]
async fn duplicates_and_strays_do_not_inflate_receipts() {
    // Mischievous echo: answers discovery properly, but echoes every RTT
    // packet twice and adds a bad-magic and a truncated packet each time.
    let socket = UdpSocket::bind(loopback(0)).await.unwrap();
    let addr = socket.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let mischief = tokio::spawn(async move {
        let mut shutdown_rx = shutdown_rx;
        let mut buf = [0u8; 2048];
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                result = socket.recv_from(&mut buf) => {
                    let Ok((n, peer)) = result else { continue };
                    if is_discovery_request(&buf[..n]) {
                        let ssrc = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
                        let reply = encode_response(ssrc, peer);
                        socket.send_to(&reply, peer).await.ok();
                    } else if decode_rtt(&buf[..n]).is_some() {
                        socket.send_to(&buf[..n], peer).await.ok();
                        socket.send_to(&buf[..n], peer).await.ok();
                        socket.send_to(&[0u8; 16], peer).await.ok();
                        socket.send_to(&[0u8; 3], peer).await.ok();
                    }
                }
            }
        }
    });

    let evidence = run_probe(&short_probe(addr)).await.unwrap();

    shutdown_tx.send(()).ok();
    mischief.await.unwrap();
    assert!(evidence.discovery_ok);
    assert!(evidence.sent > 0);
    assert_eq!(evidence.received, evidence.sent);
    assert_eq!(evidence.loss_pct, 0.0);
}

#[tokio::test]
async fn discovery_response_carries_the_echo_observed_source() {
    // End-to-end shape check: the echo's reply decodes against the probe's
    // SSRC and names a loopback source.
    let (addr, shutdown, echo) = spawn_echo().await;
    let probe = UdpSocket::bind(loopback(0)).await.unwrap();
    let request = two_bot_voice_probe::discovery::encode_request(0x000D_15C0);
    probe.send_to(&request, addr).await.unwrap();

    let mut buf = [0u8; 2048];
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), probe.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();

    shutdown.send(()).ok();
    echo.await.unwrap();
    let response = decode_response(&buf[..n], 0x000D_15C0).unwrap();
    assert_eq!(response.source.ip(), IpAddr::from(Ipv4Addr::LOCALHOST));
}

#[tokio::test]
async fn ipv6_loopback_probe_succeeds() {
    let Ok(socket) = UdpSocket::bind("[::1]:0").await else {
        eprintln!("skipping: IPv6 loopback unavailable");
        return;
    };
    let addr = socket.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let echo = tokio::spawn(async move {
        run_echo_on(socket, async move {
            shutdown_rx.await.ok();
        })
        .await
        .unwrap()
    });

    let evidence = run_probe(&short_probe(addr)).await.unwrap();

    shutdown_tx.send(()).ok();
    echo.await.unwrap();
    assert!(evidence.discovery_ok);
    let egress = evidence.observed_egress.unwrap();
    assert!(egress.starts_with("[::1]:"), "egress was {egress}");
    assert_eq!(evidence.loss_pct, 0.0);
}
