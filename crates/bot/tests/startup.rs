//! Exercise the real entrypoint and TCP listener with synthetic configuration.
//! Missing gateway prerequisites park; configured database failures exit safely.

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
        .env(
            "DATABASE_URL",
            "postgres://fixture-user:fixture-db-secret@agent-testdb/db?api_key=fixture-query-secret",
        )
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
    assert!(
        logs.contains("database initialization failed"),
        "child logs: {logs}"
    );
    // No `container service failed` here: a database_init failure exits before
    // the service_supervisor phase, which is the only place that logs it.
    for secret in ["fixture-user", "fixture-db-secret", "fixture-query-secret"] {
        assert!(!logs.contains(secret), "startup diagnostic leaked: {logs}");
    }
}

#[test]
fn configured_database_initialization_failure_exits_nonzero_without_logging_url() {
    for url in [
        "not-postgres://fixture-secret",
        "postgresql://[fixture-secret",
    ] {
        let mut child = command("127.0.0.1:0")
            .env("DISCORD_TOKEN", "INVALID")
            .env("GUILD_ID", "123")
            .env("DATABASE_URL", url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("configured DB failure parked instead of exiting");
            }
            thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        let logs = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(logs.contains("database initialization failed"));
        assert!(!logs.contains("fixture-secret"));
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
        serde_json::json!([
            ["process", "ready"],
            ["gateway", "down"],
            ["database", "down"],
            ["token_invalid", "ready"]
        ])
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

fn signal_term(child: &Child) {
    // Bash's builtin works in slim test containers without /bin/kill; the PID is
    // an inert positional argument, never interpolated into shell code.
    let status = Command::new("/bin/bash")
        .args([
            "-c",
            "kill -TERM \"$1\"",
            "signal-child",
            &child.id().to_string(),
        ])
        .status()
        .expect("SIGTERM command");
    assert!(status.success());
}

#[test]
fn second_signal_abandons_a_stalled_drain() {
    let reserved = TcpListener::bind("127.0.0.1:0").expect("reserve test port");
    let addr = reserved.local_addr().unwrap();
    drop(reserved);
    let mut bot = Bot(command(&format!("127.0.0.1:{}", addr.port()))
        .env("SHUTDOWN_TIMEOUT_SECONDS", "600")
        .spawn()
        .expect("start test bot"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while get(addr, "/health").is_err() {
        assert!(Instant::now() < deadline, "bot did not serve health");
        thread::sleep(Duration::from_millis(20));
    }

    // A request whose headers never finish keeps graceful HTTP shutdown waiting.
    let mut stalled = TcpStream::connect(addr).expect("stalled connection");
    stalled
        .write_all(b"GET /health HTTP/1.1\r\nHost: l")
        .unwrap();
    thread::sleep(Duration::from_millis(100));

    signal_term(&bot.0);
    thread::sleep(Duration::from_millis(500));
    assert!(
        bot.0.try_wait().unwrap().is_none(),
        "first signal must start a bounded drain, not exit"
    );

    signal_term(&bot.0);
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = bot.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "second signal must exit immediately"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(1));
}
