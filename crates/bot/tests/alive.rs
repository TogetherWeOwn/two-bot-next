//! Legacy "is it alive" acceptance against the actual compiled entrypoint.
//! Opt-in disposable Postgres + loopback Discord only; never runtime credentials.

#[path = "common/database_guard.rs"]
mod database_guard;

use std::{net::SocketAddr, panic::AssertUnwindSafe, process::Stdio, sync::Arc, time::Duration};

use futures_util::{FutureExt as _, SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, ConnectOptions as _, PgPool};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWriteExt as _, BufReader},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    sync::{mpsc, Mutex},
    task::JoinHandle,
    time::{sleep, timeout, Instant},
};
use tokio_websockets::{Message, ServerBuilder};
use two_bot_core::database_tls::TlsPolicy;
use two_bot_cutover::gateway_session::GatewaySessionStore;

const STEP: Duration = Duration::from_secs(5);
const TOTAL: Duration = Duration::from_secs(45);
const GUILD: &str = "2222";
const SESSION: &str = "alive-mock-session";
type Logs = Arc<Mutex<String>>;

struct TestDb {
    admin: PgPool,
    pool: PgPool,
    schema: String,
    child_url: String,
}

impl TestDb {
    async fn new() -> Self {
        let options = database_guard::test_options(); // Guard BEFORE any connection.
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(STEP)
            .connect_with(options.clone())
            .await
            .expect("disposable database connection");
        let schema = format!(
            "alive_test_{}_{}",
            std::process::id(),
            two_bot_core::funnel::now_millis_for_test()
        );
        assert!(schema
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .expect("isolated schema");
        // sqlx's to_url_lossy does NOT serialize options. Explicitly put the
        // search_path in the URL so the harness migration bootstrap and the
        // child binary both stay inside our schema.
        let mut url = options.to_url_lossy();
        url.query_pairs_mut()
            .append_pair("options[search_path]", &schema);
        let child_url = url.to_string();
        // The gateway binary is DML-only and never migrates: the harness
        // performs the operator's migration step before spawning the child,
        // exactly like the documented production bootstrap.
        two_bot_cutover::connect_with_tls(&child_url, 1, false, TlsPolicy::LocalOnly)
            .await
            .expect("operator-equivalent migration bootstrap")
            .close()
            .await;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(STEP)
            .connect(&child_url)
            .await
            .expect("schema-scoped observer");
        Self {
            admin,
            pool,
            schema,
            child_url,
        }
    }

    fn store(&self) -> GatewaySessionStore {
        GatewaySessionStore::new(self.pool.clone(), GUILD.to_owned(), 0)
    }

    async fn close(self) {
        self.pool.close().await;
        // Website jobs apply the companion contract schema (`<schema>_web_v1`)
        // on first tick; drop both generated schemas so no companion schema
        // or helper functions leak. Companion may not exist if jobs never fired.
        for schema in [&self.schema, &format!("{}_web_v1", self.schema)] {
            assert!(schema
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_'));
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP SCHEMA IF EXISTS {schema} CASCADE"
            )))
            .execute(&self.admin)
            .await
            .expect("drop only our generated schemas");
        }
        self.admin.close().await;
    }
}

struct Bot {
    child: Child,
    readers: Vec<JoinHandle<()>>,
}

impl Bot {
    fn spawn(db: &TestDb, addr: SocketAddr, gateway: &str, api: &str, logs: &Logs) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_two-bot"))
            .env_clear()
            .env("LISTEN_ADDR", addr.to_string())
            .env("DISCORD_TOKEN", "alive-synthetic-token")
            .env("GUILD_ID", GUILD)
            .env("DATABASE_URL", &db.child_url)
            // Website jobs open the same CI-service URL through cutover.
            .env("TWO_DATABASE_TLS", "local-only")
            .env("DISCORD_GATEWAY_URL", gateway)
            .env("DISCORD_API_BASE", api)
            .env("RUST_LOG", "two_bot=info")
            .env("LOG_FORMAT", "json")
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("start actual two-bot binary");
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let readers = vec![
            tokio::spawn(capture(stdout, logs.clone())),
            tokio::spawn(capture(stderr, logs.clone())),
        ];
        Self { child, readers }
    }

    fn assert_alive(&mut self) {
        assert!(
            self.child.try_wait().expect("child status").is_none(),
            "bot exited early"
        );
    }

    async fn terminate(&mut self) {
        self.assert_alive();
        let pid = self.child.id().expect("running child PID");
        // Bash's builtin works even in slim test containers without /bin/kill.
        // PID is an inert positional argument, never interpolated into shell code.
        let signal = Command::new("/bin/bash")
            .args(["-c", "kill -TERM \"$1\"", "signal-child", &pid.to_string()])
            .status()
            .await
            .expect("SIGTERM command");
        assert!(signal.success());
        let status = timeout(STEP, self.child.wait())
            .await
            .expect("SIGTERM exit deadline")
            .unwrap();
        assert!(status.success(), "SIGTERM must exit cleanly, got {status}");
        self.drain().await;
    }

    async fn cleanup(&mut self) {
        let _ = self.child.start_kill();
        let _ = timeout(STEP, self.child.wait()).await;
        self.drain().await;
    }

    async fn drain(&mut self) {
        for reader in self.readers.drain(..) {
            timeout(STEP, reader)
                .await
                .expect("child log drain deadline")
                .expect("log reader");
        }
    }
}

async fn capture(stream: impl AsyncRead + Unpin, logs: Logs) {
    let mut lines = BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await.expect("child log pipe") {
        let mut logs = logs.lock().await;
        logs.push_str(&line);
        logs.push('\n');
    }
}

struct MockDiscord {
    url: String,
    api: String,
    auth: mpsc::Receiver<Value>,
    release: mpsc::Sender<()>,
    rest_requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
    rest_task: JoinHandle<()>,
}

impl MockDiscord {
    async fn new() -> Self {
        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_addr = ws_listener.local_addr().unwrap();
        let url = format!("ws://{ws_addr}");
        // Dedicated REST socket: onboarding identity and website jobs share
        // DISCORD_API_BASE, never the gateway's two boot accept slots. No
        // `/api/v10` suffix — the executor appends the version to this origin.
        let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", rest_listener.local_addr().unwrap());
        let (auth_tx, auth) = mpsc::channel(2);
        let (release, mut gates) = mpsc::channel(4);
        let resume_url = url.clone();
        let task = tokio::spawn(async move {
            for boot in 0..2 {
                let (stream, _) = ws_listener.accept().await.unwrap();
                let (_, mut ws) = ServerBuilder::new()
                    .accept(stream)
                    .await
                    .expect("mock websocket");
                gates.recv().await.expect("release HELLO");
                ws.send(Message::text(
                    json!({"op":10,"d":{"heartbeat_interval":45000}}).to_string(),
                ))
                .await
                .unwrap();
                while let Some(message) = ws.next().await {
                    let Ok(message) = message else {
                        break;
                    }; // Child exits close its transport.
                    if message.is_close() {
                        break;
                    }
                    if !message.is_text() {
                        continue;
                    }
                    let packet: Value = serde_json::from_str(message.as_text().unwrap()).unwrap();
                    match packet["op"].as_u64() {
                        Some(1) => ws
                            .send(Message::text("{\"op\":11,\"d\":null}".to_owned()))
                            .await
                            .unwrap(),
                        Some(2 | 6) => {
                            auth_tx.send(packet).await.unwrap();
                            gates.recv().await.expect("release READY/RESUMED");
                            if boot == 0 {
                                ws.send(Message::text(json!({"op":0,"s":1,"t":"READY","d":{
                                    "v":10,"user":{"id":"999","username":"mock-bot","discriminator":"0","mfa_enabled":false},
                                    "session_id":SESSION,"resume_gateway_url":resume_url,"guilds":[],"application":{"id":"1111","flags":0}
                                }}).to_string())).await.unwrap();
                            }
                            // On restart this is the exact already-committed dispatch replay.
                            ws.send(Message::text(json!({"op":0,"s":2,"t":"GUILD_MEMBER_REMOVE","d":{
                                "guild_id":GUILD,"user":{"id":"77","username":"mock-member","discriminator":"0"}
                            }}).to_string())).await.unwrap();
                            if boot == 1 {
                                ws.send(Message::text(
                                    json!({"op":0,"s":3,"t":"RESUMED","d":{}}).to_string(),
                                ))
                                .await
                                .unwrap();
                            }
                        }
                        _ => {}
                    }
                }
            }
        });
        // Dedicated REST double: answers the website jobs' paced reads with
        // valid payloads and records arrivals so the test can prove a job
        // tick happened during the first boot before restart/resume.
        let rest_requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let rest_task = tokio::spawn(serve_rest(rest_listener, rest_requests.clone()));
        Self {
            url,
            api,
            auth,
            release,
            rest_requests,
            task,
            rest_task,
        }
    }

    async fn authentication(&mut self) -> Value {
        timeout(STEP, self.auth.recv())
            .await
            .expect("authentication deadline")
            .expect("authentication packet")
    }
}

/// Dedicated REST double on its own socket: answers the binary's paced
/// website-job reads and records every arrival. The gateway listener must
/// never see a plain-HTTP connection — one queued job tick consumed its
/// second accept slot and broke resume (exact-head CI: `Upgrade(
/// MissingHeader("Upgrade"))` at accept, then `authentication packet`).
async fn serve_rest(listener: TcpListener, recorded: Arc<Mutex<Vec<String>>>) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            break;
        };
        let recorded = recorded.clone();
        tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            let mut head = Vec::new();
            loop {
                let Ok(n) = stream.read(&mut chunk).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                head.extend_from_slice(&chunk[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if head.len() > 64 * 1024 {
                    return;
                }
            }
            let request_line = String::from_utf8_lossy(&head)
                .lines()
                .next()
                .unwrap_or("")
                .to_owned();
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_owned();
            recorded.lock().await.push(path.clone());
            // Serve the custom-command identity probes, onboarding's boot
            // identity probe and website event reads on one REST socket; the
            // fixture grounds no raid windows and unknown routes fail closed.
            let (status, body): (&str, &[u8]) =
                if request_line.starts_with("GET ") && path == "/api/v10/users/@me" {
                    ("200 OK", br#"{"id":"999","bot":true}"#)
                } else if path == "/api/v10/applications/@me" {
                    ("200 OK", b"{\"id\":\"1111\"}")
                } else if path == "/api/v10/guilds/2222" {
                    ("200 OK", b"{\"id\":\"2222\",\"name\":\"Alive fixture\"}")
                } else if path == "/api/v10/applications/1111/guilds/2222/commands"
                    || path.contains("scheduled-events")
                {
                    ("200 OK", b"[]")
                } else {
                    (
                        "404 Not Found",
                        b"{\"message\":\"alive mock: unknown route\"}",
                    )
                };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len(),
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.write_all(body).await;
        });
    }
}

async fn get(addr: SocketAddr, path: &str) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(addr).await?;
    stream
        .write_all(format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes())
        .await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    Ok(response)
}

async fn wait_http(bot: &mut Bot, addr: SocketAddr, path: &str, code: u16) -> String {
    timeout(STEP, async {
        loop {
            bot.assert_alive();
            if let Ok(Ok(response)) = timeout(Duration::from_millis(250), get(addr, path)).await {
                if response.starts_with(&format!("HTTP/1.0 {code}")) {
                    return response;
                }
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{path} did not return {code} before deadline"))
}

async fn checkpoint(db: &TestDb, seq: u64) {
    timeout(STEP, async {
        loop {
            if db
                .store()
                .load()
                .await
                .unwrap()
                .is_some_and(|session| session.sequence == seq)
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("durable checkpoint deadline");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        rows, 1,
        "restart must not duplicate persisted funnel effects"
    );
}

async fn lifecycle(db: &TestDb, discord: &mut MockDiscord, bots: &mut Vec<Bot>, logs: &Logs) {
    let reserved = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = reserved.local_addr().unwrap();
    drop(reserved);
    for boot in 0..2 {
        // Restart with an unreachable bootstrap URL: RESUME must select the
        // unchanged persisted endpoint rather than quietly IDENTIFY again.
        let gateway = if boot == 0 {
            &discord.url
        } else {
            "ws://127.0.0.1:1"
        };
        bots.push(Bot::spawn(db, addr, gateway, &discord.api, logs));
        let bot = bots.last_mut().unwrap();
        let health = wait_http(bot, addr, "/healthz", 200).await;
        assert!(health.contains("\"status\":\"ok\""));
        wait_http(bot, addr, "/health", 200).await;
        let before = wait_http(bot, addr, "/readyz", 503).await;
        assert!(before.contains("\"gateway\",\"starting\""));
        discord.release.send(()).await.unwrap(); // Health precedes HELLO.
        let auth = discord.authentication().await;
        // The DML-only binary has now finished checkpoint loading against the
        // harness-migrated schema.
        assert_eq!(
            db.store()
                .load()
                .await
                .unwrap()
                .map(|session| session.sequence),
            if boot == 0 { None } else { Some(2) }
        );
        assert_eq!(auth["op"], if boot == 0 { 2 } else { 6 });
        if boot == 1 {
            assert_eq!(auth["d"]["session_id"], SESSION);
            assert_eq!(auth["d"]["seq"], 2);
        }
        wait_http(bot, addr, "/readyz", 503).await; // Authentication alone isn't ready.
        assert_eq!(
            logs.lock()
                .await
                .lines()
                .filter(|line| line.contains("gateway ready; checkpoint committed"))
                .count(),
            boot,
            "no ready log before READY/RESUMED"
        );
        discord.release.send(()).await.unwrap();
        checkpoint(db, if boot == 0 { 2 } else { 3 }).await;
        let ready = wait_http(bot, addr, "/readyz", 200).await;
        assert!(ready.contains("\"gateway\",\"ready\""));
        if boot == 0 {
            // Deterministic website-job coverage: the events mirror must tick
            // on its own REST socket during the first boot, never the gateway
            // listener. Jitter is bounded by min(cadence, 5s); STEP covers it.
            let ticked = timeout(STEP, async {
                loop {
                    if discord
                        .rest_requests
                        .lock()
                        .await
                        .iter()
                        .any(|path| path.contains("scheduled-events"))
                    {
                        break;
                    }
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            assert!(
                ticked.is_ok(),
                "events job never hit REST during first boot"
            );
        }
        sleep(Duration::from_millis(100)).await;
        bot.assert_alive();
        bot.terminate().await;
        let rebound = TcpListener::bind(addr)
            .await
            .expect("SIGTERM must release health port");
        drop(rebound);
    }
    let logs = logs.lock().await;
    assert_eq!(
        logs.matches("gateway ready; checkpoint committed").count(),
        2,
        "ready log on each boot"
    );
    assert_eq!(logs.matches("SIGTERM received; draining").count(), 2);
    assert!(
        logs.find("listening").unwrap() < logs.find("durable gateway initialized").unwrap(),
        "listener must bind before gateway initialization"
    );
    // LOG_FORMAT=json is not implemented yet (tracing-subscriber lacks json).
    // If a later binary supports it, check every emitted line, not just READY.
    if logs
        .lines()
        .next()
        .is_some_and(|line| line.starts_with('{'))
    {
        for line in logs.lines() {
            let event: Value =
                serde_json::from_str(line).expect("each child log line must be JSON");
            assert!(event.is_object());
        }
    } else {
        eprintln!("SKIP JSON log assertion: this binary does not support LOG_FORMAT=json yet");
    }
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL; CI runs this under 60 seconds"]
async fn real_binary_is_alive_and_resumes_after_sigterm() {
    let started = Instant::now();
    let db = TestDb::new().await;
    let mut discord = MockDiscord::new().await;
    let logs = Logs::default();
    let mut bots = Vec::new();
    let result = AssertUnwindSafe(timeout(
        TOTAL,
        lifecycle(&db, &mut discord, &mut bots, &logs),
    ))
    .catch_unwind()
    .await;
    // Retain ownership outside catch_unwind: reap BOTH subprocesses and drop
    // only this schema even when an assertion or the overall deadline fails.
    for bot in &mut bots {
        bot.cleanup().await;
    }
    discord.task.abort();
    discord.rest_task.abort();
    let _ = discord.task.await;
    let _ = discord.rest_task.await;
    db.close().await;
    match result {
        Ok(Ok(())) => eprintln!("real-binary lifecycle PASS in {:?}", started.elapsed()),
        other => {
            eprintln!(
                "=== captured two-bot child logs ===\n{}=== end child logs ===",
                logs.lock().await
            );
            match other {
                Err(panic) => std::panic::resume_unwind(panic),
                _ => panic!("real-binary lifecycle exceeded {TOTAL:?}"),
            }
        }
    }
    assert!(started.elapsed() < Duration::from_secs(60));
}
