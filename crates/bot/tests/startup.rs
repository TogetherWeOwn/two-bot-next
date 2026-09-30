//! Exercise the real entrypoint and TCP listener with synthetic configuration.
//! Invalid gateway config must park the shard without moving the HTTP listener.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Bot(Child);

impl Drop for Bot {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(listen_addr: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_two-bot"));
    command
        .env_clear()
        .env("LISTEN_ADDR", listen_addr)
        .env("GUILD_ID", "not-a-snowflake")
        .env("DISCORD_TOKEN", "synthetic-token-must-not-connect")
        .env("DATABASE_URL", "synthetic-database-must-not-connect")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn get(addr: SocketAddr, path: &str) -> std::io::Result<String> {
    let timeout = Duration::from_millis(250);
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(stream, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

#[test]
fn invalid_guild_preserves_custom_listener_and_gateway_down() {
    // Reserve a non-default local port; no deployed service is contacted.
    let reserved = TcpListener::bind("127.0.0.1:0").expect("reserve test port");
    let addr = reserved.local_addr().unwrap();
    assert_ne!(addr.port(), 8080);
    let listen_addr = format!("0.0.0.0:{}", addr.port());
    drop(reserved);

    let mut bot = Bot(command(&listen_addr).spawn().expect("start test bot"));
    let deadline = Instant::now() + Duration::from_secs(5);
    let health = loop {
        assert!(
            bot.0.try_wait().unwrap().is_none(),
            "bot exited before serving the configured port"
        );
        if let Ok(response) = get(addr, "/health") {
            break response;
        }
        assert!(
            Instant::now() < deadline,
            "bot did not serve health on the configured non-default port"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert!(health.starts_with("HTTP/1.0 200"));

    let readyz = get(addr, "/readyz").expect("readiness on the same port");
    assert!(readyz.starts_with("HTTP/1.0 503"));
    let (_, body) = readyz.split_once("\r\n\r\n").expect("HTTP response body");
    let report: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(
        report["components"],
        serde_json::json!([["process", "ready"], ["gateway", "down"]])
    );

    let mut probe = Bot(command(&listen_addr).arg("--healthcheck").spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = probe.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "Docker healthcheck must use the same listener"
            );
            break;
        }
        assert!(Instant::now() < deadline, "healthcheck did not exit");
        thread::sleep(Duration::from_millis(20));
    }
}
