//! Probe and echo runtimes.
//!
//! [`run_probe`] binds an ephemeral socket, runs the discovery exchange,
//! then sends timestamped RTT packets for the configured window while
//! receiving replies, and returns the [`Evidence`].
//! Discovery failure and packet loss are evidence, not errors: only an
//! unresolvable echo target or a socket failure fails the run.
//!
//! [`run_echo`] serves one echo server: discovery-shaped replies for
//! discovery requests, verbatim echo for everything else.

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::net::UdpSocket;
use tokio::time::{timeout, Instant};

use crate::discovery::{decode_response, encode_request, encode_response, is_discovery_request};
use crate::evidence::Evidence;
use crate::stats::{loss_pct, summarize_rtt};

/// Magic word opening every RTT packet (`TWOP` in ASCII).
pub const RTT_MAGIC: u32 = 0x5457_4F50;

/// Wire length of an RTT packet: magic, sequence, send timestamp.
pub const RTT_PACKET_LEN: usize = 16;

/// Default discovery SSRC (`PROB` in ASCII). The echo is stateless, so a
/// fixed default is safe; the flag overrides it.
pub const DEFAULT_SSRC: u32 = 0x5052_4F42;

/// Receive buffer: discovery (74 B) and RTT (16 B) packets are small, and
/// opaque echo payloads are truncated past this size by `recv_from`.
const RECV_BUF_LEN: usize = 2048;

/// Fatal probe failures. A missing echo or total loss is evidence, not an
/// error; these are the runs that cannot produce numbers at all.
#[derive(Debug)]
pub enum ProbeError {
    /// The echo target neither parses nor resolves; carries the detail.
    Resolve(String),
    /// Socket bind or send failed.
    Socket(std::io::Error),
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve(detail) => write!(f, "cannot resolve echo target: {detail}"),
            Self::Socket(error) => write!(f, "probe socket failed: {error}"),
        }
    }
}

impl std::error::Error for ProbeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Resolve(_) => None,
            Self::Socket(error) => Some(error),
        }
    }
}

/// Probe run configuration. [`ProbeConfig::new`] fills operational
/// defaults (10 packets/s for 300 s); tests override the fields.
#[derive(Debug, Clone)]
pub struct ProbeConfig {
    /// Echo target as given (`host:port`); echoed into the evidence.
    pub echo_host: String,
    /// Local bind address; ephemeral by default. An unspecified IP follows
    /// the echo's family (a v4-bound socket cannot reach a v6 echo); an
    /// explicit IP is honored as given.
    pub bind: SocketAddr,
    /// RTT send window.
    pub duration: Duration,
    /// RTT packets per second.
    pub rate_per_sec: u32,
    /// Discovery SSRC.
    pub ssrc: u32,
    /// Discovery request attempts before giving up.
    pub discovery_attempts: u32,
    /// Wait per discovery attempt.
    pub discovery_timeout: Duration,
    /// Receive-only grace after the send window for stragglers.
    pub drain: Duration,
}

impl ProbeConfig {
    /// Operational defaults for `echo_host`: 10 packets/s for 300 s, three
    /// discovery attempts, 2 s discovery and drain windows.
    #[must_use]
    pub fn new(echo_host: impl Into<String>) -> Self {
        Self {
            echo_host: echo_host.into(),
            bind: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            duration: Duration::from_secs(300),
            rate_per_sec: 10,
            ssrc: DEFAULT_SSRC,
            discovery_attempts: 3,
            discovery_timeout: Duration::from_secs(2),
            drain: Duration::from_secs(2),
        }
    }
}

/// Echo server configuration.
#[derive(Debug, Clone)]
pub struct EchoConfig {
    /// Address to bind (the operator's public UDP port, or loopback).
    pub bind: SocketAddr,
}

/// Echo server counters at shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EchoStats {
    /// Discovery-shaped replies sent.
    pub discovery_replies: u64,
    /// Opaque packets echoed back verbatim.
    pub echoed: u64,
}

/// Encode one RTT packet: magic, sequence, send timestamp in microseconds
/// since the Unix epoch. The echo returns it verbatim.
#[must_use]
pub fn encode_rtt(seq: u32, send_ts_micros: u64) -> [u8; RTT_PACKET_LEN] {
    let mut packet = [0u8; RTT_PACKET_LEN];
    packet[0..4].copy_from_slice(&RTT_MAGIC.to_be_bytes());
    packet[4..8].copy_from_slice(&seq.to_be_bytes());
    packet[8..16].copy_from_slice(&send_ts_micros.to_be_bytes());
    packet
}

/// Decode an RTT reply to `(sequence, send timestamp)`. Anything else —
/// wrong size, wrong magic, discovery traffic — is `None`: strays are
/// ignored, never errors.
#[must_use]
pub fn decode_rtt(buf: &[u8]) -> Option<(u32, u64)> {
    if buf.len() != RTT_PACKET_LEN {
        return None;
    }
    let magic = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != RTT_MAGIC {
        return None;
    }
    let seq = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let send_ts = u64::from_be_bytes([
        buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
    ]);
    Some((seq, send_ts))
}

/// Run one probe against `config` and return its evidence.
pub async fn run_probe(config: &ProbeConfig) -> Result<Evidence, ProbeError> {
    let started_at = now_rfc3339();
    let echo = resolve_echo(&config.echo_host)?;
    let bind = if config.bind.ip().is_unspecified() {
        probe_bind_addr(echo, config.bind.port())
    } else {
        config.bind
    };
    let socket = UdpSocket::bind(bind).await.map_err(ProbeError::Socket)?;
    let (discovery_ok, observed) = run_discovery(&socket, echo, config).await?;
    let phase = run_rtt(&socket, echo, config).await?;
    Ok(Evidence {
        discovery_ok,
        observed_egress: observed.map(|addr| addr.to_string()),
        sent: phase.sent,
        received: phase.received,
        loss_pct: loss_pct(phase.sent, phase.received),
        rtt_ms: summarize_rtt(&phase.samples_ms),
        duration_s: phase.duration_s,
        echo_host: config.echo_host.clone(),
        started_at,
    })
}

/// Bind and serve one echo server until `shutdown` resolves.
pub async fn run_echo(
    config: &EchoConfig,
    shutdown: impl Future<Output = ()>,
) -> Result<EchoStats, std::io::Error> {
    let socket = UdpSocket::bind(config.bind).await?;
    run_echo_on(socket, shutdown).await
}

/// Serve the echo protocol on an already-bound socket until `shutdown`
/// resolves. Discovery requests get shaped replies; every other packet is
/// echoed back verbatim.
pub async fn run_echo_on(
    socket: UdpSocket,
    shutdown: impl Future<Output = ()>,
) -> Result<EchoStats, std::io::Error> {
    tokio::pin!(shutdown);
    let mut stats = EchoStats {
        discovery_replies: 0,
        echoed: 0,
    };
    let mut buf = [0u8; RECV_BUF_LEN];
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            result = socket.recv_from(&mut buf) => {
                let (n, peer) = result?;
                serve_echo_packet(&socket, &buf[..n], peer, &mut stats).await?;
            }
        }
    }
    Ok(stats)
}

/// Reply to one received datagram: shaped discovery replies, verbatim echo
/// otherwise. Returns whether a reply was sent. Peers with source port 0
/// are unreplyable — answering them fails EINVAL — so they are skipped
/// instead of taking the server down with them.
async fn serve_echo_packet(
    socket: &UdpSocket,
    buf: &[u8],
    peer: SocketAddr,
    stats: &mut EchoStats,
) -> Result<bool, std::io::Error> {
    if peer.port() == 0 {
        // Unreplyable source: answering would fail EINVAL and take the
        // server down with it.
        return Ok(false);
    }
    if is_discovery_request(buf) {
        let ssrc = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let reply = encode_response(ssrc, peer);
        socket.send_to(&reply, peer).await?;
        stats.discovery_replies += 1;
    } else {
        socket.send_to(buf, peer).await?;
        stats.echoed += 1;
    }
    Ok(true)
}

/// Local bind for `echo`: the unspecified address of the echo's family on
/// `port`. A v4-bound socket cannot send to a v6 echo, so the default bind
/// follows the resolved target instead of assuming IPv4.
fn probe_bind_addr(echo: SocketAddr, port: u16) -> SocketAddr {
    let ip = if echo.is_ipv4() {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    };
    SocketAddr::new(ip, port)
}

/// Resolve the echo target to one address (DNS or literal).
fn resolve_echo(echo_host: &str) -> Result<SocketAddr, ProbeError> {
    use std::net::ToSocketAddrs as _;
    echo_host
        .to_socket_addrs()
        .map_err(|error| ProbeError::Resolve(format!("{echo_host}: {error}")))?
        .next()
        .ok_or_else(|| ProbeError::Resolve(format!("{echo_host}: no address")))
}

/// Send discovery requests until one validates or attempts run out. Send
/// failure is fatal; silence and strays just end the exchange as failed.
async fn run_discovery(
    socket: &UdpSocket,
    echo: SocketAddr,
    config: &ProbeConfig,
) -> Result<(bool, Option<SocketAddr>), ProbeError> {
    let request = encode_request(config.ssrc);
    for _ in 0..config.discovery_attempts {
        socket
            .send_to(&request, echo)
            .await
            .map_err(ProbeError::Socket)?;
        let deadline = Instant::now() + config.discovery_timeout;
        if let Some(source) = await_discovery(socket, config.ssrc, echo, deadline).await {
            return Ok((true, Some(source)));
        }
    }
    Ok((false, None))
}

/// Wait until `deadline` for a discovery response to `ssrc` from
/// `expected_peer`. Datagrams from other senders, strays, and transient
/// receive noise are skipped; only the deadline ends the wait.
async fn await_discovery(
    socket: &UdpSocket,
    ssrc: u32,
    expected_peer: SocketAddr,
    deadline: Instant,
) -> Option<SocketAddr> {
    let mut buf = [0u8; RECV_BUF_LEN];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, sender))) => {
                if sender != expected_peer {
                    continue;
                }
                if let Ok(response) = decode_response(&buf[..n], ssrc) {
                    return Some(response.source);
                }
            }
            Ok(Err(_)) => {}
            Err(_) => return None,
        }
    }
}

/// One RTT phase: counts, unique receipts, samples, send-window seconds.
struct RttPhase {
    sent: u64,
    received: u64,
    samples_ms: Vec<f64>,
    duration_s: f64,
}

/// Collect one RTT sample per first-seen sequence. Duplicates and strays
/// are ignored; only this phase's receipts count toward loss.
fn record_rtt(buf: &[u8], seen: &mut HashSet<u32>, samples_ms: &mut Vec<f64>) {
    if let Some((seq, send_ts)) = decode_rtt(buf) {
        if seen.insert(seq) {
            let rtt_us = now_micros().saturating_sub(send_ts);
            samples_ms.push(rtt_us as f64 / 1000.0);
        }
    }
}

/// Run the RTT send window plus the drain grace. Send failure is fatal;
/// only receipts from `echo` count: strays and other senders are ignored,
/// and missing packets surface as loss, not errors.
async fn run_rtt(
    socket: &UdpSocket,
    echo: SocketAddr,
    config: &ProbeConfig,
) -> Result<RttPhase, ProbeError> {
    let start = Instant::now();
    let tick = Duration::from_secs(1) / config.rate_per_sec.max(1);
    let mut interval = tokio::time::interval(tick);
    interval.tick().await;
    let mut seq: u32 = 0;
    let mut sent: u64 = 0;
    let mut seen = HashSet::new();
    let mut samples_ms = Vec::new();
    let mut buf = [0u8; RECV_BUF_LEN];
    while start.elapsed() < config.duration {
        tokio::select! {
            _ = interval.tick() => {
                let packet = encode_rtt(seq, now_micros());
                seq = seq.wrapping_add(1);
                socket.send_to(&packet, echo).await.map_err(ProbeError::Socket)?;
                sent += 1;
            }
            result = socket.recv_from(&mut buf) => {
                if let Ok((n, sender)) = result {
                    if sender == echo {
                        record_rtt(&buf[..n], &mut seen, &mut samples_ms);
                    }
                }
            }
        }
    }
    let duration_s = start.elapsed().as_secs_f64();
    let drain_until = Instant::now() + config.drain;
    while Instant::now() < drain_until {
        let remaining = drain_until.saturating_duration_since(Instant::now());
        match timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, sender))) => {
                if sender == echo {
                    record_rtt(&buf[..n], &mut seen, &mut samples_ms);
                }
            }
            _ => break,
        }
    }
    Ok(RttPhase {
        sent,
        received: seen.len() as u64,
        samples_ms,
        duration_s: (duration_s * 1000.0).round() / 1000.0,
    })
}

/// Current UTC time as RFC 3339 (`2026-10-22T10:00:00Z`).
fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("RFC 3339 formats")
}

/// Microseconds since the Unix epoch; zero when the clock predates it.
fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
    use std::time::Duration;

    use tokio::net::UdpSocket;
    use tokio::time::Instant;

    use super::*;
    use crate::discovery::{encode_request, encode_response};

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

    #[test]
    fn probe_bind_follows_echo_family() {
        let v4: SocketAddr = "192.0.2.1:9001".parse().unwrap();
        let v6: SocketAddr = "[2001:db8::1]:9001".parse().unwrap();

        assert_eq!(
            probe_bind_addr(v4, 0),
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
        );
        assert_eq!(
            probe_bind_addr(v6, 0),
            SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
        );
        assert_eq!(probe_bind_addr(v6, 45231).port(), 45231);
    }

    #[test]
    fn rtt_codec_round_trip() {
        let packet = encode_rtt(41, 1_760_000_000_000_000);

        assert_eq!(packet.len(), RTT_PACKET_LEN);
        assert_eq!(&packet[0..4], b"TWOP");
        assert_eq!(decode_rtt(&packet), Some((41, 1_760_000_000_000_000)));
    }

    #[test]
    fn rtt_decode_rejects_strays() {
        assert_eq!(decode_rtt(&[]), None);
        assert_eq!(decode_rtt(&[0u8; 15]), None);
        assert_eq!(decode_rtt(&[0u8; 17]), None);

        let mut packet = encode_rtt(1, 2);
        packet[0] = 0;
        assert_eq!(decode_rtt(&packet), None);
    }

    #[test]
    fn probe_config_new_carries_sane_defaults() {
        let config = ProbeConfig::new("echo.example:9001");

        assert_eq!(config.echo_host, "echo.example:9001");
        assert_eq!(config.bind, SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)));
        assert_eq!(config.duration, Duration::from_secs(300));
        assert_eq!(config.rate_per_sec, 10);
        assert_eq!(config.ssrc, DEFAULT_SSRC);
        assert_eq!(config.discovery_attempts, 3);
        assert_eq!(config.discovery_timeout, Duration::from_secs(2));
        assert_eq!(config.drain, Duration::from_secs(2));
    }

    #[test]
    fn probe_error_display_names_the_fault() {
        assert_eq!(
            ProbeError::Resolve("bad host".to_owned()).to_string(),
            "cannot resolve echo target: bad host"
        );
        let io = ProbeError::Socket(std::io::Error::new(std::io::ErrorKind::AddrInUse, "in use"));
        assert!(io.to_string().starts_with("probe socket failed: "));
        assert!(io.to_string().contains("in use"));
    }

    #[tokio::test]
    async fn discovery_wait_accepts_a_valid_response() {
        let probe = UdpSocket::bind(loopback(0)).await.unwrap();
        let echo = UdpSocket::bind(loopback(0)).await.unwrap();
        let response = encode_response(5, probe.local_addr().unwrap());
        echo.send_to(&response, probe.local_addr().unwrap())
            .await
            .unwrap();

        let found = await_discovery(
            &probe,
            5,
            echo.local_addr().unwrap(),
            Instant::now() + Duration::from_secs(2),
        )
        .await;

        assert_eq!(found, Some(probe.local_addr().unwrap()));
    }

    #[tokio::test]
    async fn discovery_wait_ignores_strays_then_succeeds() {
        let probe = UdpSocket::bind(loopback(0)).await.unwrap();
        let echo = UdpSocket::bind(loopback(0)).await.unwrap();
        let target = probe.local_addr().unwrap();
        // Stray: right size, wrong type. Then the real response.
        let mut stray = encode_request(5);
        echo.send_to(&stray, target).await.unwrap();
        stray[0] = 9;
        echo.send_to(&stray, target).await.unwrap();
        echo.send_to(&[0u8; 3], target).await.unwrap();
        let response = encode_response(5, target);
        echo.send_to(&response, target).await.unwrap();

        let found = await_discovery(
            &probe,
            5,
            echo.local_addr().unwrap(),
            Instant::now() + Duration::from_secs(2),
        )
        .await;

        assert_eq!(found, Some(target));
    }

    #[tokio::test]
    async fn discovery_wait_times_out() {
        let probe = UdpSocket::bind(loopback(0)).await.unwrap();

        let found = await_discovery(
            &probe,
            5,
            loopback(9),
            Instant::now() + Duration::from_millis(50),
        )
        .await;

        assert_eq!(found, None);
    }

    #[tokio::test]
    async fn discovery_wait_ignores_responses_from_other_senders() {
        let probe = UdpSocket::bind(loopback(0)).await.unwrap();
        let echo = UdpSocket::bind(loopback(0)).await.unwrap();
        let decoy = UdpSocket::bind(loopback(0)).await.unwrap();
        let target = probe.local_addr().unwrap();
        // Valid shape and SSRC, but from the wrong sender: the silent echo
        // must keep the exchange failed.
        let spoof = encode_response(5, target);
        decoy.send_to(&spoof, target).await.unwrap();

        let found = await_discovery(
            &probe,
            5,
            echo.local_addr().unwrap(),
            Instant::now() + Duration::from_millis(100),
        )
        .await;

        assert_eq!(found, None);
    }

    /// Live verbatim echo until aborted; the probe-side socket stays with
    /// the caller so decoys can target it directly.
    async fn spawn_verbatim_echo() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let socket = UdpSocket::bind(loopback(0)).await.unwrap();
        let addr = socket.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut buf = [0u8; RECV_BUF_LEN];
            loop {
                let Ok((n, peer)) = socket.recv_from(&mut buf).await else {
                    break;
                };
                if socket.send_to(&buf[..n], peer).await.is_err() {
                    break;
                }
            }
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn rtt_ignores_packets_from_other_senders() {
        let probe = UdpSocket::bind(loopback(0)).await.unwrap();
        let probe_addr = probe.local_addr().unwrap();
        let (echo_addr, echo_task) = spawn_verbatim_echo().await;
        // Decoys pre-seeded into the probe's buffer with sequences the run
        // never sends, so any counted decoy inflates `received` past `sent`.
        let decoy = UdpSocket::bind(loopback(0)).await.unwrap();
        for seq in 1_000_000..1_000_010u32 {
            let fake = encode_rtt(seq, now_micros());
            decoy.send_to(&fake, probe_addr).await.unwrap();
        }
        let mut config = short_probe(echo_addr);
        config.duration = Duration::from_millis(300);

        let phase = run_rtt(&probe, echo_addr, &config).await.unwrap();
        echo_task.abort();

        assert!(phase.sent > 0);
        assert_eq!(phase.received, phase.sent);
    }

    #[tokio::test]
    async fn rtt_drain_ignores_packets_from_other_senders() {
        let probe = UdpSocket::bind(loopback(0)).await.unwrap();
        let probe_addr = probe.local_addr().unwrap();
        let (echo_addr, echo_task) = spawn_verbatim_echo().await;
        // Decoys land mid-drain: the 300 ms send window at 100/s establishes
        // sends unconditionally, and the 400 ms decoys fall inside the ~300
        // to ~600 ms drain instead of racing the first send tick.
        let decoy = UdpSocket::bind(loopback(0)).await.unwrap();
        let decoy_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            for seq in 2_000_000..2_000_005u32 {
                let fake = encode_rtt(seq, now_micros());
                decoy.send_to(&fake, probe_addr).await.ok();
            }
        });
        let mut config = short_probe(echo_addr);
        config.rate_per_sec = 100;
        config.duration = Duration::from_millis(300);
        config.drain = Duration::from_millis(300);

        let phase = run_rtt(&probe, echo_addr, &config).await.unwrap();
        echo_task.abort();
        decoy_task.await.unwrap();

        assert!(phase.sent > 0);
        assert_eq!(phase.received, phase.sent);
    }

    #[tokio::test]
    async fn probe_fails_on_unresolvable_echo() {
        let config = short_probe(loopback(9));
        let config = ProbeConfig {
            echo_host: "not a host!!".to_owned(),
            ..config
        };

        let result = run_probe(&config).await;

        assert!(matches!(result, Err(ProbeError::Resolve(_))));
    }

    #[tokio::test]
    async fn probe_fails_when_bind_port_is_occupied() {
        let held = StdUdpSocket::bind(loopback(0)).unwrap();
        let port = held.local_addr().unwrap().port();
        let mut config = short_probe(loopback(9));
        config.bind = loopback(port);

        let result = run_probe(&config).await;

        assert!(matches!(result, Err(ProbeError::Socket(_))));
    }

    #[tokio::test]
    async fn echo_skips_port_zero_peer_and_keeps_serving() {
        let socket = UdpSocket::bind(loopback(0)).await.unwrap();
        let client = UdpSocket::bind(loopback(0)).await.unwrap();
        let mut stats = EchoStats {
            discovery_replies: 0,
            echoed: 0,
        };
        // A datagram from source port 0 is unreplyable: no reply is
        // attempted, so no EINVAL can escape, and the stats stay untouched.
        let port_zero = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let served = serve_echo_packet(&socket, b"ping", port_zero, &mut stats)
            .await
            .unwrap();
        assert!(!served);
        assert_eq!(stats.discovery_replies, 0);
        assert_eq!(stats.echoed, 0);
        // The echo still serves the next real peer.
        let served = serve_echo_packet(&socket, b"ping", client.local_addr().unwrap(), &mut stats)
            .await
            .unwrap();
        assert!(served);
        assert_eq!(stats.echoed, 1);
        let mut buf = [0u8; 64];
        let (n, _) = timeout(Duration::from_secs(2), client.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], b"ping");
    }

    #[tokio::test]
    async fn echo_fails_when_bind_port_is_occupied() {
        let held = StdUdpSocket::bind(loopback(0)).unwrap();
        let port = held.local_addr().unwrap().port();
        let config = EchoConfig {
            bind: loopback(port),
        };

        let result = run_echo(&config, async {}).await;

        assert!(result.is_err());
    }
}
