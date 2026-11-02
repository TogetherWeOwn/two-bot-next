//! Binary acceptance for `voice-probe`: help, usage errors, a loopback
//! probe run printing evidence JSON, and fatal-error exit codes.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use two_bot_voice_probe::discovery::{decode_response, encode_request};

fn voice_probe() -> Command {
    Command::new(env!("CARGO_BIN_EXE_voice-probe"))
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

/// Spawned-echo port for the loopback probe test. Below the default Linux
/// ephemeral range, so `:0` binds never hand it out; the child echo is the
/// only user. (Binding `:0` and releasing races parallel tests: another
/// test can grab the port before the child binds it.)
const ECHO_PORT: u16 = 18751;

/// A child process reaped on drop, so a failing test never leaks an echo.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

/// Block until the echo on `addr` answers discovery (up to ~5 s).
fn wait_for_echo(addr: SocketAddr) {
    let socket = StdUdpSocket::bind(loopback(0)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let request = encode_request(0xBEAC);
    let mut buf = [0u8; 2048];
    for _ in 0..25 {
        socket.send_to(&request, addr).ok();
        if let Ok((n, _)) = socket.recv_from(&mut buf) {
            if decode_response(&buf[..n], 0xBEAC).is_ok() {
                return;
            }
        }
    }
    panic!("echo on {addr} never answered discovery");
}

#[test]
fn help_exits_zero_with_usage() {
    let output = voice_probe().arg("--help").output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: voice-probe"), "was: {stdout}");
    assert!(output.stderr.is_empty());
}

#[test]
fn usage_error_exits_two() {
    let output = voice_probe().arg("--bogus").output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(voice_probe().arg("--echo").output().unwrap().status.code() == Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("voice-probe: "), "was: {stderr}");
    assert!(stderr.contains("Usage: voice-probe"), "was: {stderr}");
}

#[test]
fn probe_loopback_prints_evidence_and_exits_zero() {
    let port = ECHO_PORT;
    let echo = ChildGuard(
        voice_probe()
            .args(["--mode", "echo", "--bind", &format!("127.0.0.1:{port}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_echo(loopback(port));

    let output = voice_probe()
        .args([
            "--echo",
            &format!("127.0.0.1:{port}"),
            "--duration-s",
            "1",
            "--rate-s",
            "5",
        ])
        .output()
        .unwrap();

    drop(echo);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["discovery_ok"], true);
    assert!(value["observed_egress"]
        .as_str()
        .unwrap()
        .starts_with("127.0.0.1:"));
    assert_eq!(value["loss_pct"], 0.0);
    assert!(value["sent"].as_u64().unwrap() > 0);
    assert_eq!(value["sent"], value["received"]);
}

#[test]
fn probe_unresolvable_echo_exits_one() {
    // Non-numeric service: passes the host:port shape check, then fails in
    // the resolver without touching the network.
    let output = voice_probe()
        .args(["--echo", "127.0.0.1:notaport"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("cannot resolve echo target"),
        "was: {stderr}"
    );
}

#[test]
fn echo_occupied_port_exits_one() {
    let held = StdUdpSocket::bind(loopback(0)).unwrap();
    let port = held.local_addr().unwrap().port();

    let output = voice_probe()
        .args(["--mode", "echo", "--bind", &format!("127.0.0.1:{port}")])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("echo failed"), "was: {stderr}");
}
