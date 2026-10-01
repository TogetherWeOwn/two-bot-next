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
    assert_parked_gateway(&[
        ("DISCORD_TOKEN", "INVALID"),
        ("DATABASE_URL", "synthetic-database-must-not-connect"),
        ("GUILD_ID", "not-a-snowflake"),
    ]);
}

#[test]
fn missing_database_with_token_parks_gateway() {
    assert_parked_gateway(&[("DISCORD_TOKEN", "INVALID"), ("GUILD_ID", "123")]);
}

#[test]
fn empty_database_with_token_parks_gateway() {
    assert_parked_gateway(&[
        ("DISCORD_TOKEN", "INVALID"),
        ("DATABASE_URL", ""),
        ("GUILD_ID", "123"),
    ]);
}

#[test]
fn missing_guild_with_token_parks_gateway() {
    assert_parked_gateway(&[
        ("DISCORD_TOKEN", "INVALID"),
        ("DATABASE_URL", "synthetic-database-must-not-connect"),
    ]);
}

#[test]
fn zero_guild_with_token_parks_gateway() {
    assert_parked_gateway(&[
        ("DISCORD_TOKEN", "INVALID"),
        ("DATABASE_URL", "synthetic-database-must-not-connect"),
        ("GUILD_ID", "0"),
    ]);
}

#[test]
fn missing_token_parks_gateway() {
    assert_parked_gateway(&[
        ("DATABASE_URL", "synthetic-database-must-not-connect"),
        ("GUILD_ID", "123"),
    ]);
}

#[test]
fn empty_token_parks_gateway() {
    assert_parked_gateway(&[
        ("DISCORD_TOKEN", ""),
        ("DATABASE_URL", "synthetic-database-must-not-connect"),
        ("GUILD_ID", "123"),
    ]);
}

#[test]
fn configured_gateway_initialization_failure_exits_nonzero() {
    // A malformed synthetic URL fails locally; no database or Discord is contacted.
    let mut bot = Bot(command("127.0.0.1:0")
        .env("DISCORD_TOKEN", "INVALID")
        .env("DATABASE_URL", "postgres://fixture-user:fixture-db-secret@agent-testdb/db?api_key=fixture-query-secret")
        .env("GUILD_ID", "123")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start test bot"));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = bot.0.try_wait().unwrap() {
            assert_eq!(status.code(), Some(1));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "configured failed gateway stayed alive"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let mut logs = String::new();
    bot.0.stdout.take().unwrap().read_to_string(&mut logs).unwrap();
    bot.0.stderr.take().unwrap().read_to_string(&mut logs).unwrap();
    assert!(logs.contains("database_connect_failed"), "child logs: {logs}");
    assert!(logs.contains("container_service_failed"), "child logs: {logs}");
    for secret in ["fixture-user", "fixture-db-secret", "fixture-query-secret"] {
        assert!(!logs.contains(secret), "startup diagnostic leaked: {logs}");
    }
}

#[cfg(unix)]
#[test]
fn non_unicode_gateway_override_exits_without_logging_its_value() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

    let override_url = OsString::from_vec(b"ws://127.0.0.1:1/synthetic-secret-\xff".to_vec());
    let mut bot = Bot(command("127.0.0.1:0")
        .env("DISCORD_TOKEN", "INVALID")
        .env("DATABASE_URL", "synthetic-database-must-not-connect")
        .env("GUILD_ID", "123")
        .env("DISCORD_GATEWAY_URL", override_url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start test bot"));
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = bot.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "invalid gateway override stayed alive"
        );
        thread::sleep(Duration::from_millis(20));
    };
    let mut logs = String::new();
    bot.0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut logs)
        .unwrap();
    bot.0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut logs)
        .unwrap();
    assert_eq!(status.code(), Some(1), "child logs: {logs}");
    assert!(
        logs.contains("DISCORD_GATEWAY_URL must be valid UTF-8"),
        "child logs: {logs}"
    );
    assert!(
        !logs.contains("synthetic-secret"),
        "override value leaked: {logs}"
    );
    assert!(
        !logs.contains("durable gateway"),
        "gateway initialized before rejection: {logs}"
    );
}

fn assert_parked_gateway(vars: &[(&str, &str)]) {
    // Reserve a non-default local port; no deployed service is contacted.
    let reserved = TcpListener::bind("127.0.0.1:0").expect("reserve test port");
    let addr = reserved.local_addr().unwrap();
    assert_ne!(addr.port(), 8080);
    let listen_addr = format!("0.0.0.0:{}", addr.port());
    drop(reserved);

    let mut bot = Bot(command(&listen_addr)
        .envs(vars.iter().copied())
        .spawn()
        .expect("start test bot"));
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

    for name in ["counter", "rank", "scheduled_events"] {
        assert_eq!(report["jobs"][name]["parked"], true);
        assert_eq!(report["jobs"][name]["running"], false);
        assert_eq!(report["jobs"][name]["last_start"], serde_json::Value::Null);
    }

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
