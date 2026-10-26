//! Exercise the real entrypoint and TCP listener with synthetic configuration.
//! Missing gateway prerequisites park; configured database failures exit safely.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
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

fn forward_lines(reader: impl Read + Send + 'static, lines: mpsc::Sender<String>) {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                return;
            }
        }
    });
}

fn reserve_eventsub_bind() -> String {
    let reserved = TcpListener::bind("127.0.0.1:0").expect("reserve test port");
    let bind = reserved.local_addr().unwrap().to_string();
    drop(reserved);
    bind
}

/// A failed eventsub bind is the only retryable early exit: the
/// reserve/drop/rebind handoff is racy at full parallelism and another test
/// process can claim the port before the child binds it, so the child exits
/// with `receiver_bind_failed` before the reconcile task spawns. Any other
/// early exit is a genuine regression and panics at once.
fn is_eventsub_bind_failure(logs: &str) -> bool {
    logs.contains("receiver_bind_failed")
}

/// A bind failure is retryable only while attempts remain: on the last
/// attempt it must fall through to the fail-loud panic with its child logs,
/// mirroring `should_retry_park_attempt` for the parked-gateway loop.
fn should_retry_eventsub_attempt(logs: &str, attempt: u8, attempts: u8) -> bool {
    attempt < attempts && is_eventsub_bind_failure(logs)
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or_default()
}

/// One eventsub-reconcile attempt: spawn the entrypoint with the given bind
/// and wait for the startup pass to log its config refusal. Returns `None`
/// only when the child failed to bind and attempts remain; panics with the
/// child logs on a timeout, a last-attempt bind failure, or any other early
/// exit so genuine regressions never become a retried or silent pass.
fn eventsub_reconcile_once(bind: &str, attempt: u8, attempts: u8) -> Option<()> {
    let mut child = command("127.0.0.1:0")
        .env("DISCORD_TOKEN", "INVALID")
        .env("GUILD_ID", "123")
        .env("TWO_EVENTSUB", "1")
        .env("TWO_EVENTSUB_BIND", bind)
        .env("TWO_TWITCH_WEBHOOK_SECRET", "synthetic-webhook-secret-01")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start test bot");
    let (lines_tx, lines_rx) = mpsc::channel();
    forward_lines(child.stdout.take().unwrap(), lines_tx.clone());
    forward_lines(child.stderr.take().unwrap(), lines_tx);
    let _bot = Bot(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut logs = String::new();
    loop {
        match lines_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(line) => {
                if line.contains("eventsub_reconcile") && line.contains("reconcile_config_invalid")
                {
                    return Some(());
                }
                logs.push_str(&line);
                logs.push('\n');
            }
            Err(mpsc::RecvTimeoutError::Timeout) => panic!(
                "startup reconcile pass never logged its refusal within 10s; child logs: {logs}"
            ),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Both forwarder threads ended at pipe EOF, so every line the
                // child wrote is already in `logs`.
                if should_retry_eventsub_attempt(&logs, attempt, attempts) {
                    return None;
                }
                panic!("bot exited before the startup pass logged its refusal; child logs: {logs}");
            }
        }
    }
}

fn assert_eventsub_reconcile_with_binds(next_bind: &mut dyn FnMut() -> String) {
    const ATTEMPTS: u8 = 3;
    for attempt in 1..=ATTEMPTS {
        if eventsub_reconcile_once(&next_bind(), attempt, ATTEMPTS).is_some() {
            return;
        }
        assert!(
            attempt < ATTEMPTS,
            "eventsub bind failed on its last allowed attempt"
        );
        // Bind failure: the port was stolen under parallel load. The exited
        // child is reaped on drop; the next attempt reserves a fresh bind.
    }
    // Unreachable while `eventsub_reconcile_once` panics on the last attempt,
    // but keep the fallthrough loud in its own right: exhausting the retries
    // without seeing the reconcile refusal is a failure, never a pass.
    panic!("eventsub reconcile attempts exhausted without the startup refusal");
}

#[test]
fn eventsub_reconcile_startup_pass_runs_from_the_entrypoint() {
    // The gate is open but no callback is configured, so the startup pass
    // refuses before any Helix call. That refusal log is the proof the
    // entrypoint spawned the reconcile task.
    assert_eventsub_reconcile_with_binds(&mut reserve_eventsub_bind);
}

#[test]
fn eventsub_stolen_bind_retries_with_fresh_port() {
    // Deterministically steal the first attempt's bind: with no retry this
    // test fails (bind failure panics instead of retrying), pinning the loop.
    let held = TcpListener::bind("127.0.0.1:0").expect("hold test port");
    let held_bind = held.local_addr().unwrap().to_string();
    let calls = std::cell::Cell::new(0u8);
    let mut provider = || {
        let n = calls.get() + 1;
        calls.set(n);
        if n == 1 {
            held_bind.clone()
        } else {
            reserve_eventsub_bind()
        }
    };
    assert_eventsub_reconcile_with_binds(&mut provider);
    assert!(
        calls.get() >= 2,
        "stolen-bind test never took its second attempt"
    );
}

#[test]
fn eventsub_retry_gates_only_on_receiver_bind_failure() {
    assert!(is_eventsub_bind_failure(
        "error_class=\"receiver_bind_failed\" error_class=\"eventsub startup refused\""
    ));
    assert!(!is_eventsub_bind_failure("database initialization failed"));
    assert!(!is_eventsub_bind_failure(""));
    assert!(should_retry_eventsub_attempt(
        "error_class=\"receiver_bind_failed\"",
        1,
        3
    ));
    assert!(!should_retry_eventsub_attempt(
        "error_class=\"receiver_bind_failed\"",
        3,
        3
    ));
    assert!(!should_retry_eventsub_attempt(
        "database initialization failed",
        1,
        3
    ));
}

#[test]
fn eventsub_bind_exhaustion_panics_with_child_logs() {
    // Pin the loop's fail-loud exit: every attempt's bind is held, so all
    // three children die with receiver_bind_failed and the last one must
    // panic carrying its logs, never fall out as a silent pass.
    let held = TcpListener::bind("127.0.0.1:0").expect("hold test port");
    let held_bind = held.local_addr().unwrap().to_string();
    let calls = std::cell::Cell::new(0u8);
    let mut provider = || {
        calls.set(calls.get() + 1);
        held_bind.clone()
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_eventsub_reconcile_with_binds(&mut provider);
    }));
    let payload = outcome.expect_err("exhausted bind retries must panic, never pass silently");
    assert_eq!(
        calls.get(),
        3,
        "exhaustion must stop after the third attempt"
    );
    let message = panic_message(&*payload);
    assert!(
        message.contains("receiver_bind_failed"),
        "the last attempt's child logs must ride the panic: {message}"
    );
    assert!(
        message.contains("bot exited before the startup pass"),
        "exhaustion must fail loud, not drift into the timeout arm: {message}"
    );
}

#[test]
fn eventsub_non_bind_early_exit_panics_without_retry() {
    // Pin the non-bind fail-fast path: port 0 is a config refusal
    // (receiver_config_invalid, no bind attempted), so the first child's
    // early exit must panic at once instead of consuming retries.
    let calls = std::cell::Cell::new(0u8);
    let mut provider = || {
        calls.set(calls.get() + 1);
        "127.0.0.1:0".to_string()
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_eventsub_reconcile_with_binds(&mut provider);
    }));
    let payload = outcome.expect_err("a non-bind early exit must panic at once");
    assert_eq!(calls.get(), 1, "a non-bind early exit must not retry");
    let message = panic_message(&*payload);
    assert!(
        message.contains("receiver_config_invalid"),
        "the child's refusal must ride the panic: {message}"
    );
    assert!(
        !message.contains("receiver_bind_failed"),
        "a config refusal must not be misread as a bind failure: {message}"
    );
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

fn reserve_test_port() -> (SocketAddr, String) {
    let reserved = TcpListener::bind("127.0.0.1:0").expect("reserve test port");
    let addr = reserved.local_addr().unwrap();
    assert_ne!(addr.port(), 8080);
    let listen_addr = format!("0.0.0.0:{}", addr.port());
    drop(reserved);
    (addr, listen_addr)
}

/// A parked child that cannot bind its listener exits 1 with this marker
/// (crates/bot/src/main.rs `listener_bind_failed`). Only that case is
/// retried: any other early exit is a genuine regression and panics at once.
fn is_listener_bind_failure(logs: &str) -> bool {
    logs.contains("listener_bind_failed")
}

fn should_retry_park_attempt(logs: &str, attempt: u8, attempts: u8) -> bool {
    attempt < attempts && is_listener_bind_failure(logs)
}

/// Drain piped child logs until the forwarder threads end at pipe EOF.
/// Both senders live in those threads, so disconnect means every line the
/// child wrote has been collected; a `try_recv` drain can race the flush
/// and report empty logs.
fn drain_logs(rx: mpsc::Receiver<String>) -> String {
    let mut logs = String::new();
    for line in rx {
        logs.push_str(&line);
        logs.push('\n');
    }
    logs
}

/// One parked-gateway attempt: spawn on the given port and wait for health.
/// Returns `None` when the child failed to bind (retryable); panics on any
/// other early exit so genuine regressions never become a retried pass.
fn park_once(
    addr: SocketAddr,
    listen_addr: &str,
    vars: &[(&str, &str)],
    attempt: u8,
    attempts: u8,
) -> Option<(String, Bot, mpsc::Receiver<String>)> {
    let mut child = command(listen_addr)
        .envs(vars.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start test bot");
    let (lines_tx, lines_rx) = mpsc::channel();
    forward_lines(child.stdout.take().unwrap(), lines_tx.clone());
    forward_lines(child.stderr.take().unwrap(), lines_tx);
    let mut bot = Bot(child);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = bot.0.try_wait().unwrap() {
            let logs = drain_logs(lines_rx);
            if should_retry_park_attempt(&logs, attempt, attempts) {
                return None;
            }
            panic!(
                "bot exited with {status} before serving the configured port; child logs: {logs}"
            );
        }
        if let Ok(response) = get(addr, "/health") {
            // The health response must come from the spawned child: a sibling
            // parked bot on a stolen port serves identical /health and /readyz
            // bodies, so an exited child here means the response is not ours.
            if let Some(status) = bot.0.try_wait().unwrap() {
                let logs = drain_logs(lines_rx);
                if should_retry_park_attempt(&logs, attempt, attempts) {
                    return None;
                }
                panic!(
                    "bot exited with {status} around the health probe; response may be from another process; child logs: {logs}"
                );
            }
            return Some((response, bot, lines_rx));
        }
        assert!(
            Instant::now() < deadline,
            "bot did not serve health on the configured non-default port"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn assert_parked_gateway(vars: &[(&str, &str)]) {
    // Reserve a non-default local port; no deployed service is contacted.
    // The reserve/drop/rebind handoff is racy at full parallelism: another
    // test process can claim the port before the child binds it, and the
    // child then exits on listener bind failure. Retry with a fresh port so
    // a stolen port cannot flake the suite. Child output is piped (not
    // nulled) so a genuine early exit still reports its cause.
    assert_parked_gateway_with_ports(vars, &mut reserve_test_port);
}

fn assert_parked_gateway_with_ports(
    vars: &[(&str, &str)],
    next_port: &mut dyn FnMut() -> (SocketAddr, String),
) {
    const ATTEMPTS: u8 = 3;
    let mut attempt = 0;
    let (addr, listen_addr, health, mut bot, _log_rx) = loop {
        attempt += 1;
        let (addr, listen_addr) = next_port();
        if let Some((health, bot, log_rx)) = park_once(addr, &listen_addr, vars, attempt, ATTEMPTS)
        {
            break (addr, listen_addr, health, bot, log_rx);
        }
        assert!(
            attempt < ATTEMPTS,
            "park attempt failed to bind on its last allowed attempt"
        );
        // Bind failure: the port was stolen under parallel load. The exited
        // child is reaped here; the next attempt reserves a fresh port.
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

    for name in [
        "counter",
        "rank",
        "scheduled_events",
        "settings",
        "presence_probe",
        "community_scorecard",
        "inactivity",
    ] {
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

    // The readyz and healthcheck assertions above must have run against the
    // spawned child: a sibling on a stolen port serves identical bodies.
    assert!(
        bot.0.try_wait().unwrap().is_none(),
        "parked child exited before the end of the assertions"
    );
}

#[test]
fn park_retry_gates_only_on_listener_bind_failure() {
    assert!(is_listener_bind_failure(
        "startup_phase=\"listener_bind\" error_class=\"listener_bind_failed\""
    ));
    assert!(!is_listener_bind_failure("database initialization failed"));
    assert!(!is_listener_bind_failure(""));
    assert!(should_retry_park_attempt(
        "error_class=\"listener_bind_failed\"",
        1,
        3
    ));
    assert!(!should_retry_park_attempt(
        "error_class=\"listener_bind_failed\"",
        3,
        3
    ));
    assert!(!should_retry_park_attempt(
        "database initialization failed",
        1,
        3
    ));
}

#[test]
fn stolen_port_retries_with_fresh_port() {
    // Deterministically steal the first attempt's port: with ATTEMPTS=1 this
    // test fails (bind failure panics instead of retrying), pinning the loop.
    let held = TcpListener::bind("127.0.0.1:0").expect("hold test port");
    let held_addr = held.local_addr().unwrap();
    let held_listen = format!("0.0.0.0:{}", held_addr.port());
    let calls = std::cell::Cell::new(0u8);
    let mut provider = || {
        let n = calls.get() + 1;
        calls.set(n);
        if n == 1 {
            (held_addr, held_listen.clone())
        } else {
            reserve_test_port()
        }
    };
    assert_parked_gateway_with_ports(
        &[("DISCORD_TOKEN", "INVALID"), ("GUILD_ID", "123")],
        &mut provider,
    );
    assert!(
        calls.get() >= 2,
        "stolen-port test never took its second attempt"
    );
}

fn fake_readyz_body() -> String {
    serde_json::json!({
        "components": [
            ["process", "ready"],
            ["gateway", "down"],
            ["database", "down"],
            ["token_invalid", "ready"]
        ],
        "jobs": {
            "counter": {"parked": true, "running": false, "last_start": null},
            "rank": {"parked": true, "running": false, "last_start": null},
            "scheduled_events": {"parked": true, "running": false, "last_start": null},
            "settings": {"parked": true, "running": false, "last_start": null},
            "presence_probe": {"parked": true, "running": false, "last_start": null},
            "community_scorecard": {"parked": true, "running": false, "last_start": null},
            "inactivity": {"parked": true, "running": false, "last_start": null}
        }
    })
    .to_string()
}

/// Serve canned parked-gateway bodies on a stolen port. The listener stays
/// bound (so the own child bind-dies) while accepting connections. Health
/// responses wait 180ms so the bind-doomed child has exited before the
/// response arrives; the wait stays under `get`'s 250ms timeout. Set
/// `shutdown` to stop the thread.
fn spawn_fake_parked_server(
    listener: TcpListener,
    shutdown: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        listener.set_nonblocking(true).expect("fake nonblocking");
        let readyz_body = fake_readyz_body();
        while !shutdown.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
                    let mut buf = [0u8; 2048];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                    if request.contains("GET /readyz") {
                        let response = format!(
                            "HTTP/1.0 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                            readyz_body.len(),
                            readyz_body
                        );
                        let _ = stream.write_all(response.as_bytes());
                    } else if request.contains("GET /health") {
                        thread::sleep(Duration::from_millis(180));
                        let body = "OK";
                        let response = format!(
                            "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes());
                    } else {
                        let body = "not found";
                        let response = format!(
                            "HTTP/1.0 404 Not Found\r\nContent-Length: {}\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    let _ = stream.flush();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    })
}

struct StopFakeOnDrop(Arc<AtomicBool>);

impl Drop for StopFakeOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn stolen_port_with_serving_sibling_retries_on_attribution() {
    // Pins the child-attribution checks: a sibling serving identical /health
    // and /readyz bodies on the stolen port must not satisfy the first
    // attempt. The own child bind-dies during the delayed health response;
    // the post-health alive check must notice and retry on a fresh port.
    // With both alive checks deleted, attempt one accepts the fake response
    // and the provider is called exactly once.
    let listener = TcpListener::bind("127.0.0.1:0").expect("hold test port");
    let held_addr = listener.local_addr().unwrap();
    let held_listen = format!("0.0.0.0:{}", held_addr.port());
    let shutdown = Arc::new(AtomicBool::new(false));
    let server = spawn_fake_parked_server(listener, Arc::clone(&shutdown));
    let _stop = StopFakeOnDrop(Arc::clone(&shutdown));

    let calls = std::cell::Cell::new(0u8);
    let mut provider = || {
        let n = calls.get() + 1;
        calls.set(n);
        if n == 1 {
            (held_addr, held_listen.clone())
        } else {
            reserve_test_port()
        }
    };
    assert_parked_gateway_with_ports(
        &[("DISCORD_TOKEN", "INVALID"), ("GUILD_ID", "123")],
        &mut provider,
    );
    shutdown.store(true, Ordering::SeqCst);
    let _ = server.join();
    assert!(
        calls.get() >= 2,
        "serving-sibling test never retried to a fresh port"
    );
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
