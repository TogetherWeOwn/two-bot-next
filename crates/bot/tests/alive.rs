//! Legacy "is it alive" acceptance against the actual compiled entrypoint.
//! Opt-in disposable Postgres + loopback Discord only; never runtime credentials.

#[path = "common/database_guard.rs"]
mod database_guard;

#[allow(dead_code)]
#[path = "../../discord/tests/common/mod.rs"]
mod common;
use common::RestRequest;

use std::{
    cell::Cell,
    net::SocketAddr,
    panic::AssertUnwindSafe,
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering::SeqCst},
        Arc,
    },
    time::Duration,
};

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
        Self::new_with_claim_fault(Self::new_schema_name(), false).await
    }

    async fn new_with_claim_fault(schema: String, lose_commit_ack: bool) -> Self {
        let options = database_guard::test_options(); // Guard BEFORE any connection.
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(STEP)
            .connect_with(options.clone())
            .await
            .expect("disposable database connection");
        let created = Self::claim_schemas(&admin, &schema, lose_commit_ack).await;
        if !matches!(created, Ok(Ok(()))) {
            // A failed or timed-out COMMIT has an uncertain outcome. Do not
            // drop either name without proof that this transaction committed.
            eprintln!("schema setup failed or was uncertain; retain {schema} and its companion");
            if matches!(created, Ok(Err(sqlx::Error::Protocol(ref message)))
                if message == "injected lost commit acknowledgement")
            {
                panic!("injected lost commit acknowledgement after schema claim");
            }
            panic!("isolated schema creation failed");
        }
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
        // Bootstrap can fail after opening a connection, before returning its
        // pool. Treat its whole lifetime as uncertain until close completes.
        let mut migration_close_uncertain = true;
        let setup = AssertUnwindSafe(async {
            let migration_pool =
                two_bot_cutover::connect_with_tls(&child_url, 1, false, TlsPolicy::LocalOnly)
                    .await
                    .expect("operator-equivalent migration bootstrap");
            timeout(STEP, migration_pool.close())
                .await
                .expect("migration pool close deadline");
            migration_close_uncertain = false;
            PgPoolOptions::new()
                .max_connections(2)
                .acquire_timeout(STEP)
                .connect(&child_url)
                .await
                .expect("schema-scoped observer")
        })
        .catch_unwind()
        .await;
        let pool = match setup {
            Ok(pool) => pool,
            Err(panic) => {
                if migration_close_uncertain {
                    eprintln!("migration pool did not close; retain {schema} and its companion");
                    let _ = timeout(STEP, admin.close()).await;
                    std::panic::resume_unwind(panic);
                }
                let cleanup_failed = Self::drop_schemas(&admin, &schema).await;
                let admin_closed = timeout(STEP, admin.close()).await.is_ok();
                assert!(!cleanup_failed, "drop schemas after setup failure");
                assert!(
                    admin_closed,
                    "admin pool close deadline after setup failure"
                );
                std::panic::resume_unwind(panic);
            }
        };
        Self {
            admin,
            pool,
            schema,
            child_url,
        }
    }

    fn new_schema_name() -> String {
        static NEXT_SCHEMA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        format!(
            "alive_test_{}_{}_{}",
            std::process::id(),
            two_bot_core::funnel::now_millis_for_test(),
            NEXT_SCHEMA.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    }

    async fn claim_schemas(
        admin: &PgPool,
        schema: &str,
        lose_commit_ack: bool,
    ) -> Result<Result<(), sqlx::Error>, tokio::time::error::Elapsed> {
        assert!(schema
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        // Claim both names in one transaction; never drop names after an
        // unsuccessful or uncertain claim.
        timeout(STEP, async {
            let mut transaction = admin.begin().await?;
            for name in [schema, &format!("{schema}_web_v1")] {
                sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {name}")))
                    .execute(&mut *transaction)
                    .await?;
            }
            transaction.commit().await?;
            if lose_commit_ack {
                return Err(sqlx::Error::Protocol(
                    "injected lost commit acknowledgement".into(),
                ));
            }
            Ok(())
        })
        .await
    }

    fn store(&self) -> GatewaySessionStore {
        GatewaySessionStore::new(self.pool.clone(), GUILD.to_owned(), 0)
    }

    async fn close(self) {
        if timeout(STEP, self.pool.close()).await.is_err() {
            eprintln!(
                "observer pool did not close; retain {} and its companion",
                self.schema
            );
            let _ = timeout(STEP, self.admin.close()).await;
            panic!("observer pool close deadline");
        }
        // Website jobs apply the companion contract schema (`<schema>_web_v1`)
        // on first tick; drop both generated schemas so no companion schema
        // or helper functions leak. Companion may not exist if jobs never fired.
        let cleanup_failed = Self::drop_schemas(&self.admin, &self.schema).await;
        let admin_closed = timeout(STEP, self.admin.close()).await.is_ok();
        assert!(!cleanup_failed, "drop only our generated schemas");
        assert!(admin_closed, "admin pool close deadline");
    }

    async fn drop_schemas(admin: &PgPool, schema: &str) -> bool {
        let mut cleanup_failed = false;
        for schema in [schema, &format!("{schema}_web_v1")] {
            assert!(schema
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_'));
            let dropped = timeout(
                STEP,
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DROP SCHEMA IF EXISTS {schema} CASCADE"
                )))
                .execute(admin),
            )
            .await;
            if !matches!(dropped, Ok(Ok(_))) {
                cleanup_failed = true;
            }
        }
        cleanup_failed
    }
}

struct Bot {
    child: Child,
    readers: Vec<JoinHandle<()>>,
}

impl Bot {
    fn spawn(db: &TestDb, addr: SocketAddr, gateway: Option<&str>, api: &str, logs: &Logs) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_two-bot"));
        command.env_clear();
        // No gateway override is a strict-boot probe: onboarding must reject.
        match gateway {
            Some(gateway) => command.env("DISCORD_GATEWAY_URL", gateway),
            None => command.env("TWO_ONBOARDING_MODE", "invalid-probe"),
        };
        let mut child = command
            .env("LISTEN_ADDR", addr.to_string())
            .env("DISCORD_TOKEN", "alive-synthetic-token")
            .env("GUILD_ID", GUILD)
            .env("DATABASE_URL", &db.child_url)
            // Website jobs open the same CI-service URL through cutover.
            .env("TWO_DATABASE_TLS", "local-only")
            .env("DISCORD_API_BASE", api)
            .env("TWO_ANNOUNCEMENTS", "1")
            .env("TWO_COMMUNITY_SCORECARD", "1")
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

    async fn cleanup(&mut self) -> bool {
        let _ = self.child.start_kill();
        let mut complete = matches!(timeout(STEP, self.child.wait()).await, Ok(Ok(_)));
        let mut pending = Vec::new();
        for mut reader in self.readers.drain(..) {
            match timeout(STEP, &mut reader).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    eprintln!("child log reader failed: {error}");
                    complete = false;
                }
                Err(_) => {
                    complete = false;
                    reader.abort();
                    if timeout(STEP, &mut reader).await.is_err() {
                        pending.push(reader);
                    }
                }
            }
        }
        self.readers = pending;
        complete
    }

    async fn drain(&mut self) {
        while !self.readers.is_empty() {
            let result = timeout(STEP, &mut self.readers[0])
                .await
                .expect("child log drain deadline");
            drop(self.readers.remove(0));
            result.expect("log reader");
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
    handshakes: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
    rest_requests: Arc<Mutex<Vec<RestRequest>>>,
    connections: Arc<AtomicUsize>,
    task: JoinHandle<()>,
    rest_task: JoinHandle<()>,
    rest_handlers: Arc<Mutex<tokio::task::JoinSet<()>>>,
}

impl MockDiscord {
    async fn new() -> Self {
        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_addr = ws_listener.local_addr().unwrap();
        let url = format!("ws://{ws_addr}");
        // Registry publication is HTTP before gateway connect on BOTH boots.
        // Onboarding identity and website jobs share DISCORD_API_BASE, so use
        // a route-aware REST socket separate from the gateway: job reads must
        // neither consume scripted registry replies nor be mistaken for
        // websocket upgrades. No `/api/v10` suffix — the executor appends
        // the version to this origin itself.
        let rest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", rest_listener.local_addr().unwrap());
        let (auth_tx, auth) = mpsc::channel(2);
        let (handshake_tx, handshakes) = mpsc::channel(2);
        let (release, mut gates) = mpsc::channel(4);
        let resume_url = url.clone();
        let connections = Arc::new(AtomicUsize::new(0));
        let accepted = connections.clone();
        let task = tokio::spawn(async move {
            for boot in 0..2 {
                let (stream, _) = ws_listener.accept().await.unwrap();
                accepted.fetch_add(1, SeqCst);
                let (_, mut ws) = ServerBuilder::new()
                    .accept(stream)
                    .await
                    .expect("mock websocket");
                handshake_tx.send(()).await.expect("handshake receiver");
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
        // Dedicated REST double: records registry bodies and website-job
        // reads so both full publication and a first-boot tick are observable.
        let rest_requests: Arc<Mutex<Vec<RestRequest>>> = Arc::default();
        let rest_handlers = Arc::new(Mutex::new(tokio::task::JoinSet::new()));
        let rest_task = tokio::spawn(serve_rest(
            rest_listener,
            rest_requests.clone(),
            rest_handlers.clone(),
        ));
        Self {
            url,
            api,
            auth,
            handshakes,
            release,
            rest_requests,
            connections,
            task,
            rest_task,
            rest_handlers,
        }
    }

    async fn stop(self) -> bool {
        let mut complete = true;
        for task in [self.task, self.rest_task] {
            task.abort();
            complete &= match timeout(STEP, task).await {
                Ok(Ok(())) => true,
                Ok(Err(error)) => error.is_cancelled(),
                Err(_) => false,
            };
        }
        let mut handlers = self.rest_handlers.lock().await;
        handlers.abort_all();
        let joined = timeout(STEP, async {
            while let Some(result) = handlers.join_next().await {
                complete &= result.is_ok() || result.is_err_and(|error| error.is_cancelled());
            }
        })
        .await;
        complete && joined.is_ok()
    }

    async fn authentication(&mut self) -> Value {
        timeout(STEP, self.auth.recv())
            .await
            .expect("authentication deadline")
            .expect("authentication packet")
    }
}

/// Dedicated REST double on its own socket: answers registry publication and
/// paced website-job reads by route, recording full request bodies. The gateway
/// listener must never see plain HTTP — a queued job tick previously consumed
/// its second accept slot and broke resume with `MissingHeader("Upgrade")`.
async fn serve_rest(
    listener: TcpListener,
    recorded: Arc<Mutex<Vec<RestRequest>>>,
    handlers: Arc<Mutex<tokio::task::JoinSet<()>>>,
) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            break;
        };
        let recorded = recorded.clone();
        handlers.lock().await.spawn(async move {
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
            let head_end = head.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            let request_head = String::from_utf8_lossy(&head[..head_end]);
            let mut lines = request_head.lines();
            let mut request_line = lines.next().unwrap_or("").split_whitespace();
            let method = request_line.next().unwrap_or("").to_owned();
            let path = request_line.next().unwrap_or("").to_owned();
            let headers: Vec<(String, String)> = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
                .collect();
            let content_len = headers
                .iter()
                .find(|(name, _)| name == "content-length")
                .and_then(|(_, value)| value.parse::<usize>().ok())
                .unwrap_or(0);
            if content_len > 1024 * 1024 {
                return;
            }
            let mut request_body = head[head_end..].to_vec();
            while request_body.len() < content_len {
                let Ok(n) = stream.read(&mut chunk).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                request_body.extend_from_slice(&chunk[..n]);
            }
            request_body.truncate(content_len);
            // Copy before the record takes ownership so the registry PUT can
            // echo the submitted command list as its JSON receipt.
            let registry_echo = request_body.clone();
            recorded.lock().await.push(RestRequest {
                method: method.clone(),
                path: path.clone(),
                headers,
                body: request_body,
                received_at: tokio::time::Instant::now(),
            });
            // Registry and job requests can interleave, so never script replies
            // by arrival order. The fixture grounds no raid windows; only the
            // events mirror reads. Unknown routes fail closed on this socket.
            // Also serves onboarding's boot identity probe on this REST socket.
            let (status, body): (&str, Vec<u8>) = match (method.as_str(), path.as_str()) {
                ("GET", "/api/v10/applications/@me") => ("200 OK", b"{\"id\":\"1111\"}".to_vec()),
                ("GET", "/api/v10/users/@me") => {
                    ("200 OK", b"{\"id\":\"999\",\"bot\":true}".to_vec())
                }
                ("GET", "/api/v10/guilds/2222") => (
                    "200 OK",
                    b"{\"id\":\"2222\",\"name\":\"Alive fixture\"}".to_vec(),
                ),
                ("PUT", "/api/v10/applications/1111/guilds/2222/commands") => {
                    ("200 OK", registry_echo)
                }
                ("GET", path) if path.contains("scheduled-events") => ("200 OK", b"[]".to_vec()),
                _ => (
                    "404 Not Found",
                    b"{\"message\":\"alive mock: unknown route\"}".to_vec(),
                ),
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len(),
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.write_all(&body).await;
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
            // Bounded 10 ms backoff under the outer STEP deadline (~100
            // polls/s): throttles the /readyz round trips instead of
            // busy-spinning on scheduler wakeups.
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
            // Bounded 10 ms backoff under the outer STEP deadline: spaces
            // the store loads against the same Postgres the bot writes to.
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

/// Count lines carrying a stable `msg` catalog name (`docs/logging.md`). The
/// readiness line keeps its human "gateway ready; checkpoint committed"
/// message for the dashboards that match it, but tests key on `msg`. Each boot
/// emits exactly one durable READY/RESUMED commit (`ready` on fresh IDENTIFY,
/// `gateway_resumed` on RESUME replay) and one `shutdown_started` on SIGTERM.
fn msg_count(logs: &str, names: &[&str]) -> usize {
    logs.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| {
            event
                .get("msg")
                .and_then(Value::as_str)
                .is_some_and(|msg| names.contains(&msg))
        })
        .count()
}

fn ready_commits(logs: &str) -> usize {
    msg_count(logs, &["ready", "gateway_resumed"])
}

/// Every durable READY/RESUMED commit must carry gateway correlation
/// (`docs/logging.md`): the decimal-string guild id plus the boot run id. The
/// emit site runs on the dispatch `spawn_blocking` worker, which inherits no
/// span context, so the serving task's gateway span is re-entered per
/// dispatch; these assertions prove it through the real binary.
fn assert_ready_correlation(logs: &str) {
    let mut seen = 0;
    for line in logs.lines() {
        let event: Value = match serde_json::from_str(line) {
            Ok(event) => event,
            Err(_) => continue,
        };
        let is_ready = event
            .get("msg")
            .and_then(Value::as_str)
            .is_some_and(|msg| msg == "ready" || msg == "gateway_resumed");
        if !is_ready {
            continue;
        }
        seen += 1;
        assert_eq!(
            event.get("guild_id").and_then(Value::as_str),
            Some(GUILD),
            "ready log must carry the decimal-string guild id"
        );
        assert!(
            event
                .get("run_id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty()),
            "ready log must carry the boot run id"
        );
    }
    assert!(seen > 0, "expected at least one ready commit in logs");
}

async fn seed_raw(db: &TestDb, url: &str) {
    sqlx::query("INSERT INTO gateway_sessions (guild_id, shard_id, session_id, seq, resume_url, updated_at) VALUES ($1,0,$2,42,$3,now()) ON CONFLICT (guild_id,shard_id) DO UPDATE SET resume_url=EXCLUDED.resume_url")
        .bind(GUILD).bind(SESSION).bind(url).execute(&db.pool).await.unwrap();
}

async fn raw_url(db: &TestDb) -> Option<String> {
    sqlx::query_scalar("SELECT resume_url FROM gateway_sessions WHERE guild_id=$1 AND shard_id=0")
        .bind(GUILD)
        .fetch_optional(&db.pool)
        .await
        .unwrap()
}

async fn strict_boot_probes(db: &TestDb, api: &str, bots: &mut Vec<Bot>, logs: &Logs, saved: &str) {
    for url in [saved, "wss://remote.invalid"] {
        logs.lock().await.clear();
        seed_raw(db, url).await;
        let addr = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        bots.push(Bot::spawn(db, addr, None, api, logs));
        let bot = bots.last_mut().unwrap();
        timeout(STEP, async {
            while !logs.lock().await.lines().any(|line| {
                line.contains("\"error_class\":\"onboarding_gates_invalid\"")
                    && line.contains("\"failed_step\":\"onboarding_gates\"")
            }) {
                bot.assert_alive();
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("positive post-load onboarding failure deadline");
        assert!(db.store().load().await.unwrap().is_none());
        assert_eq!(raw_url(db).await, None, "boot rejected raw checkpoint");
        assert!(bot.cleanup().await, "probe teardown");
    }
}

async fn lifecycle(
    db: &TestDb,
    discord: &mut MockDiscord,
    bots: &mut Vec<Bot>,
    logs: &Logs,
    decoy: Option<&str>,
) {
    let reserved = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = reserved.local_addr().unwrap();
    drop(reserved);
    // Registry-sync requests consumed by the per-boot assertion below.
    let mut consumed = 0usize;
    for boot in 0..2 {
        // Restart with an unreachable bootstrap URL: RESUME must select the
        // unchanged persisted endpoint rather than quietly IDENTIFY again.
        let gateway = if boot == 0 {
            &discord.url
        } else {
            decoy.unwrap_or("ws://127.0.0.1:1")
        };
        bots.push(Bot::spawn(db, addr, Some(gateway), &discord.api, logs));
        let bot = bots.last_mut().unwrap();
        let health = wait_http(bot, addr, "/healthz", 200).await;
        assert!(health.contains("\"status\":\"ok\""));
        wait_http(bot, addr, "/health", 200).await;
        let before = wait_http(bot, addr, "/readyz", 503).await;
        assert!(before.contains("\"gateway\",\"starting\""));
        discord.release.send(()).await.unwrap(); // Health precedes HELLO.
        let auth = discord.authentication().await;
        let requests: Vec<_> = discord
            .rest_requests
            .lock()
            .await
            .iter()
            .filter(|request| {
                request.path == "/api/v10/applications/@me"
                    || request.path == "/api/v10/applications/1111/guilds/2222/commands"
            })
            .cloned()
            .collect();
        // Boot performs three registry-sync requests before gateway connect,
        // all awaited before Identify so the boot-0 slice is exact: the
        // custom-command bootstrap identity, the ordered-interaction boot-sync
        // identity, then the boot-sync publication. The mock withholds READY
        // until the release below, so no READY/RESUMED publish can appear in
        // boot 0's slice. Boot 1's slice may additionally carry boot 0's
        // READY-time republication at its head; only its trailing three are
        // asserted.
        let fresh = &requests[consumed..];
        let sync = if boot == 0 {
            assert_eq!(
                fresh.len(),
                3,
                "registry sync on each boot: two identities plus one publication"
            );
            fresh
        } else {
            assert!(
                fresh.len() >= 3,
                "registry sync on each boot: two identities plus one publication"
            );
            &fresh[fresh.len() - 3..]
        };
        let bootstrap_identity = &sync[0];
        assert_eq!(bootstrap_identity.method, "GET");
        assert_eq!(bootstrap_identity.path, "/api/v10/applications/@me");
        let identity = &sync[1];
        assert_eq!(identity.method, "GET");
        assert_eq!(identity.path, "/api/v10/applications/@me");
        let publish = &sync[2];
        assert_eq!(publish.method, "PUT");
        assert_eq!(
            publish.path,
            "/api/v10/applications/1111/guilds/2222/commands"
        );
        let commands: Vec<Value> = serde_json::from_slice(&publish.body).unwrap();
        let names: Vec<_> = commands
            .iter()
            .map(|command| command["name"].as_str().unwrap())
            .collect();
        // The harness boots with a synthetic token no clearance recognizes,
        // so activation narrows every clearable surface while scorecard
        // stays: the single boot PUT carries exactly the seven core commands
        // plus scorecard, each once. Uncleared
        // surfaces stay unpublished (and refused at dispatch) rather than
        // advertised.
        assert_eq!(
            names,
            [
                "rank",
                "leaderboard",
                "help",
                "privacy",
                "reopen",
                "welcome-preview",
                "game-mix",
                "attendance"
            ],
            "publish the narrowed shared registry exactly once, nothing withheld or extra"
        );
        if boot == 1 {
            assert_eq!(
                publish.body, requests[2].body,
                "same registry on RESUMED boot"
            );
        }
        consumed += fresh.len();
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
            ready_commits(&logs.lock().await),
            boot,
            "no ready log before READY/RESUMED"
        );
        discord.release.send(()).await.unwrap();
        checkpoint(db, if boot == 0 { 2 } else { 3 }).await;
        assert_eq!(
            raw_url(db).await.as_deref(),
            Some(&*discord.url),
            "persist the actual admitted wire URL"
        );
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
                        .any(|request| request.path.contains("scheduled-events"))
                    {
                        break;
                    }
                    // Bounded 10 ms backoff under the outer STEP deadline:
                    // spaces the rest_requests lock polls during the jitter
                    // window instead of spinning on the mutex.
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            assert!(
                ticked.is_ok(),
                "events job never hit REST during first boot"
            );
        }
        // Post-ready settle window: poll liveness for 200 ms so a crash
        // just after RESUMED fails as "bot exited early" instead of being
        // masked by an immediate SIGTERM. Bounded 10 ms backoff keeps the
        // window cheap under load.
        let settle_until = tokio::time::Instant::now() + Duration::from_millis(200);
        while tokio::time::Instant::now() < settle_until {
            bot.assert_alive();
            sleep(Duration::from_millis(10)).await;
        }
        bot.assert_alive();
        assert_ready_correlation(&logs.lock().await);
        bot.terminate().await;
        let rebound = TcpListener::bind(addr)
            .await
            .expect("SIGTERM must release health port");
        drop(rebound);
    }
    let logs = logs.lock().await;
    assert_eq!(ready_commits(&logs), 2, "ready log on each boot");
    assert_ready_correlation(&logs);
    assert_eq!(
        msg_count(&logs, &["shutdown_started"]),
        2,
        "shutdown log on each SIGTERM"
    );
    assert!(
        logs.find("listening").unwrap() < logs.find("durable gateway initialized").unwrap(),
        "listener must bind before gateway initialization"
    );
    // LOG_FORMAT=json is the default (stable `msg` catalog in docs/logging.md);
    // the child runs with it above, so every emitted line must be a JSON
    // object with a string `msg`.
    assert!(logs.lines().next().is_some(), "child must emit logs");
    for line in logs.lines() {
        let event: Value = serde_json::from_str(line).expect("each child log line must be JSON");
        assert!(event.is_object());
        assert!(
            event.get("msg").and_then(Value::as_str).is_some(),
            "each child log line must carry a string msg"
        );
    }
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL; CI runs this under 60 seconds"]
async fn real_binary_is_alive_and_resumes_after_sigterm() {
    binary_lifecycle(false).await;
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL; CI runs this under 60 seconds"]
async fn real_binary_resumes_without_contacting_accepting_bootstrap_decoy() {
    binary_lifecycle(true).await;
}

async fn binary_lifecycle(accepting_decoy: bool) {
    let started = Instant::now();
    let db = TestDb::new().await;
    let mut discord = MockDiscord::new().await;
    let logs = Logs::default();
    let mut bots = Vec::new();
    let decoy = MockDiscord::new().await; // Accepting listener: any dial is counted.
    let exercise = async {
        strict_boot_probes(&db, &discord.api, &mut bots, &logs, &decoy.url).await;
        let owned = discord.connections.load(SeqCst) + decoy.connections.load(SeqCst);
        assert_eq!(owned, 0, "no override-absent listener connections");
        logs.lock().await.clear();
        discord.rest_requests.lock().await.clear();
        // Inadmissible checkpoint under a checked local bootstrap must IDENTIFY.
        seed_raw(&db, &format!("{}/invalid", decoy.url)).await;
        let bootstrap = accepting_decoy.then_some(decoy.url.as_str());
        lifecycle(&db, &mut discord, &mut bots, &logs, bootstrap).await;
        assert_eq!(
            decoy.connections.load(SeqCst),
            0,
            "no probe/decoy connections"
        );
    };
    let result = AssertUnwindSafe(timeout(TOTAL, exercise))
        .catch_unwind()
        .await;
    // Retain ownership outside catch_unwind: reap BOTH subprocesses and drop
    // only this schema even when an assertion or the overall deadline fails.
    let mut cleaned = true;
    for bot in &mut bots {
        cleaned &= bot.cleanup().await;
    }
    cleaned &= decoy.stop().await;
    cleaned &= discord.stop().await;
    if cleaned {
        db.close().await;
    } else {
        eprintln!("uncertain child/task teardown; retain owned schemas");
    }
    if !cleaned {
        eprintln!("captured child logs:\n{}", logs.lock().await);
    }
    assert!(cleaned, "all children reaped and log readers joined");
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

#[test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
fn fixture_schema_collision_preserves_existing_companion() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("fixture runtime");
    runtime.block_on(async {
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(STEP)
            .connect_with(database_guard::test_options())
            .await
            .expect("disposable database connection");
        let schema = TestDb::new_schema_name();
        let companion = format!("{schema}_web_v1");
        // Only remove this name if this test successfully created it.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {companion}")))
            .execute(&admin)
            .await
            .expect("create collision sentinel");
        let setup = AssertUnwindSafe(TestDb::new_with_claim_fault(schema.clone(), false))
            .catch_unwind()
            .await;
        let existing: Vec<String> = sqlx::query_scalar(
            "SELECT nspname FROM pg_namespace WHERE nspname = $1 OR nspname = $2 ORDER BY nspname",
        )
        .bind(&schema)
        .bind(&companion)
        .fetch_all(&admin)
        .await
        .expect("inspect collision outcome");
        // This test owns only the empty sentinel; never drop the primary name.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {companion}"
        )))
        .execute(&admin)
        .await
        .expect("remove collision sentinel");
        timeout(STEP, admin.close())
            .await
            .expect("admin close deadline");
        let panic = match setup {
            Err(panic) => panic,
            Ok(_) => panic!("collision must reject fixture setup"),
        };
        assert_eq!(
            panic.downcast_ref::<&str>().copied(),
            Some("isolated schema creation failed")
        );
        assert_eq!(
            existing,
            vec![companion],
            "claim must roll back the primary and preserve the sentinel"
        );
    });
}

#[test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
fn fixture_lost_commit_ack_retains_both_schemas() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("fixture runtime");
    runtime.block_on(async {
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(STEP)
            .connect_with(database_guard::test_options())
            .await
            .expect("disposable database connection");
        let schema = TestDb::new_schema_name();
        let setup = AssertUnwindSafe(TestDb::new_with_claim_fault(schema.clone(), true))
            .catch_unwind()
            .await;
        // Only this injected panic proves that our transaction committed. Any
        // other outcome leaves ownership uncertain and must not trigger cleanup.
        let panic = match setup {
            Err(panic) => panic,
            Ok(_) => panic!("injected setup failure did not occur"),
        };
        assert_eq!(
            panic.downcast_ref::<&str>().copied(),
            Some("injected lost commit acknowledgement after schema claim")
        );
        let verifier = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(STEP)
            .connect_with(database_guard::test_options())
            .await
            .expect("independent schema verifier");
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_namespace WHERE nspname = $1 OR nspname = $2",
        )
        .bind(&schema)
        .bind(format!("{schema}_web_v1"))
        .fetch_one(&verifier)
        .await
        .expect("inspect uncertain claim");
        assert_eq!(
            count, 2,
            "committed schemas must remain after a lost acknowledgement"
        );
        timeout(STEP, verifier.close())
            .await
            .expect("verifier close deadline");
        assert!(
            !TestDb::drop_schemas(&admin, &schema).await,
            "remove test-owned schemas"
        );
        timeout(STEP, admin.close())
            .await
            .expect("admin close deadline");
    });
}

#[tokio::test]
#[ignore = "requires the explicit agent-testdb/CI test URL"]
async fn fixture_teardown_releases_processes_sockets_and_schemas_after_each_outcome() {
    for outcome in 0..4 {
        let db = TestDb::new().await;
        let mut discord = MockDiscord::new().await;
        let addr = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let sockets = [
            addr,
            discord.url[5..].parse().unwrap(),
            discord.api[7..].parse().unwrap(),
        ];
        let schema = db.schema.clone();
        let logs = Logs::default();
        let mut bot = Bot::spawn(&db, addr, Some(&discord.url), &discord.api, &logs);
        let phase = Cell::new("health readiness");
        let exercised = AssertUnwindSafe(timeout(STEP, async {
            wait_http(&mut bot, addr, "/healthz", 200).await;
            // Health precedes gateway initialization. Finish the upgrade before
            // intentionally killing the child, but keep HELLO gated.
            phase.set("gateway upgrade");
            discord.handshakes.recv().await.expect("mock handshake");
            phase.set("injected outcome");
            match outcome {
                1 => panic!("injected fixture assertion"),
                2 => std::future::pending::<()>().await,
                _ => {}
            }
        }))
        .catch_unwind()
        .await;
        if outcome == 3 {
            // The real capture future panics on invalid UTF-8 before cleanup joins it.
            bot.readers
                .insert(0, tokio::spawn(capture(&b"\xff\n"[..], logs.clone())));
        }
        let cleaned = bot.cleanup().await;
        assert!(
            discord.stop().await,
            "mock tasks joined after reader failure"
        );
        assert!(bot.child.try_wait().unwrap().is_some(), "child was reaped");
        assert!(bot.readers.is_empty(), "reader handles consumed");
        if outcome == 3 {
            assert!(!cleaned, "completed reader panic marks teardown incomplete");
            let retained: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname=$1)")
                    .bind(&schema)
                    .fetch_one(&db.pool)
                    .await
                    .unwrap();
            assert!(retained, "incomplete cleanup retains its schema");
            assert!(
                bot.cleanup().await,
                "completed handles are never polled again"
            );
        } else {
            assert!(cleaned, "fixture cleanup after outcome {outcome}");
        }
        db.close().await;
        assert_eq!(
            exercised.is_err(),
            outcome == 1,
            "outcome {outcome}, phase {}",
            phase.get()
        );
        if let Ok(result) = exercised {
            assert_eq!(
                result.is_err(),
                outcome == 2,
                "outcome {outcome}, phase {}",
                phase.get()
            );
            if outcome == 2 {
                assert_eq!(phase.get(), "injected outcome", "timeout during startup");
            }
        }
        for socket in sockets {
            let rebound = TcpListener::bind(socket)
                .await
                .expect("owned socket released");
            drop(rebound);
        }
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(STEP)
            .connect_with(database_guard::test_options())
            .await
            .unwrap();
        let remaining: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_namespace WHERE nspname=$1 OR nspname=$2")
                .bind(&schema)
                .bind(format!("{schema}_web_v1"))
                .fetch_one(&admin)
                .await
                .unwrap();
        timeout(STEP, admin.close()).await.unwrap();
        assert_eq!(remaining, 0, "owned schemas released");
    }
}
