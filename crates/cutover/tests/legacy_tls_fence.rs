//! Threat-model F6 acceptance tests for the two legacy cutover binaries.
//!
//! `docs/database-tls.md` ("Coverage and gaps") names `legacy_copy` and
//! `legacy_verify` as the last unfenced connection paths: both build their
//! source/target pools from operator URLs without
//! `two_bot_core::database_tls::enforce`/`apply`. These tests pin the fenced
//! behaviour at the process boundary, mirroring
//! `crates/cutover/tests/secret_connection.rs`:
//!
//! - a URL the `TWO_DATABASE_TLS` policy refuses (unset means `required`)
//!   fails with the policy's fixed string and exit code 2, before SQLx parses
//!   the URL or opens a socket, and no URL part (host, user, credential,
//!   database, or the policy value itself) reaches stdout or stderr;
//! - the URL's sslmode is applied to the socket (a `disable` client speaks
//!   plaintext startup, a `require` client opens with an SSLRequest), so a
//!   fence that strips or ignores the mode instead of enforcing it fails;
//! - `legacy_copy --plan` stays offline and unaffected;
//! - against the disposable test Postgres (ignored test), the same local
//!   endpoints refuse under the default policy and verify green only with an
//!   explicit `local-only` opt-in, in both source and target positions.
//!
//! Every fixture value contains `fixture`, so one blanket assertion proves no
//! URL part was echoed. No test here contacts a real host: remote shapes use
//! synthetic Neon-shaped hostnames that a fenced binary refuses before DNS,
//! and socket-level cases use a loopback spy listener.

use std::io::{ErrorKind, Read as _, Write as _};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WEAK_SSLMODE: &str = "database sslmode does not require TLS";
const MISSING_SSLMODE: &str = "database URL must set sslmode under the required TLS policy";
const LOCAL_REFUSED: &str = "local database host is refused under the required TLS policy";
const REMOTE_REFUSED: &str = "remote database host is refused under the local-only TLS policy";
const UNSUPPORTED_SSLMODE: &str = "unsupported database sslmode";
const INVALID_SETTING: &str = "invalid TWO_DATABASE_TLS setting";

/// Bounded subprocess wait (same shape as
/// `admission_configuration::admission_bootstrap_tls_spy_refuses_before_any_socket`):
/// an unfenced binary dials the silent spy and hangs in startup, so a timeout
/// is a test failure, never a stuck suite.
fn run_bounded(command: &mut Command, label: &str) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        match child.try_wait().unwrap() {
            Some(status) => break status,
            None if Instant::now() > deadline => {
                child.kill().ok();
                let _ = child.wait();
                panic!("{label}: binary did not refuse within 20s (dialed instead?)");
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    Output {
        status,
        stdout,
        stderr,
    }
}

fn legacy_copy(source: &str, target: &str, tls: Option<&str>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_legacy_copy"));
    command
        .env_clear()
        .arg(format!("--source-url={source}"))
        .arg(format!("--target-url={target}"))
        // Reach connection construction: `--apply` plus the live-target
        // opt-in, so only the TLS policy can refuse these fixture URLs.
        .args(["--apply", "--allow-live-target"]);
    if let Some(tls) = tls {
        command.env("TWO_DATABASE_TLS", tls);
    }
    command
}

fn legacy_verify(source: &str, target: &str, tls: Option<&str>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_legacy_verify"));
    command
        .env_clear()
        .args([
            "--source-url-env",
            "TLS_FENCE_SOURCE_URL",
            "--target-url-env",
            "TLS_FENCE_TARGET_URL",
            "--mapping",
            concat!(env!("CARGO_MANIFEST_DIR"), "/mappings/example.json"),
        ])
        .env("TLS_FENCE_SOURCE_URL", source)
        .env("TLS_FENCE_TARGET_URL", target);
    if let Some(tls) = tls {
        command.env("TWO_DATABASE_TLS", tls);
    }
    command
}

/// A policy refusal is a refusal, not a connection failure: exit code 2, the
/// fixed string on stderr, and no fixture (URL) material in either stream.
/// `legacy_verify` additionally prints nothing to stdout on a refusal (the
/// precedent in `legacy_verify_db.rs`); `legacy_copy` may print its mapping
/// inventory there, so only the no-echo property is pinned for it.
fn assert_tls_refusal(output: &Output, expected: &str, label: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{label}: stderr: {stderr}");
    assert!(
        stderr.contains(expected),
        "{label}: expected fixed refusal {expected:?}, stderr: {stderr}"
    );
    if label.starts_with("legacy_verify") || label.starts_with("verify") {
        assert!(stdout.is_empty(), "{label}: stdout not empty: {stdout}");
    }
    for text in [&stdout, &stderr] {
        assert!(
            !text.contains("fixture"),
            "{label}: refusal echoed URL material: {text}"
        );
    }
}

/// Assert the kernel backlog accepted nothing: the binary refused before any
/// socket. `spy` was bound but never accepted from while the binary ran.
fn assert_no_dial(spy: &std::net::TcpListener, label: &str) {
    spy.set_nonblocking(true).unwrap();
    match spy.accept() {
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        Ok(_) => panic!("{label}: TLS fence dialed the database instead of refusing"),
        Err(error) => panic!("{label}: spy accept failed: {error}"),
    }
}

fn spy_url(port: u16, database: &str, sslmode: &str) -> String {
    format!("postgres://fixture-user:fixture-secret@127.0.0.1:{port}/{database}?sslmode={sslmode}")
}

/// Loopback plaintext under the default (`required`) policy, both URLs aimed
/// at one silent spy listener. Fenced code refuses before any socket;
/// unfenced code dials the spy and hangs in startup, which `run_bounded`
/// turns into a failure. This is the dial-discriminating entry proof.
fn required_policy_refuses_loopback_plaintext_before_any_socket(binary: &str) {
    let spy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = spy.local_addr().unwrap().port();
    let source = spy_url(port, "fixture-source", "disable");
    let target = spy_url(port, "fixture-target", "disable");
    let mut command = match binary {
        "legacy_copy" => legacy_copy(&source, &target, None),
        _ => legacy_verify(&source, &target, None),
    };
    let output = run_bounded(&mut command, binary);
    assert_tls_refusal(&output, LOCAL_REFUSED, binary);
    assert_no_dial(&spy, binary);
}

#[test]
fn legacy_copy_required_policy_refuses_loopback_plaintext_before_any_socket() {
    required_policy_refuses_loopback_plaintext_before_any_socket("legacy_copy");
}

#[test]
fn legacy_verify_required_policy_refuses_loopback_plaintext_before_any_socket() {
    required_policy_refuses_loopback_plaintext_before_any_socket("legacy_verify");
}

/// The refusal matrix from `secret_connection.rs`, driven through both
/// binaries' real entries. Both URLs carry the same defect, so whichever
/// pool the binary builds first must produce the fixed string. Remote shapes
/// are synthetic Neon-shaped hostnames: a fenced binary refuses before DNS,
/// an unfenced one fails (or hangs) with a connection error instead.
fn tls_refusal_matrix(binary: &str) {
    let remote = "postgres://fixture-user:fixture-secret@ep-fixture-host.us-east-2.aws.neon.tech";
    let cases: [(&str, Option<&str>, String, String, &str); 5] = [
        (
            "weak sslmode under required",
            None,
            format!("{remote}/fixture-source?sslmode=disable"),
            format!("{remote}/fixture-target?sslmode=disable"),
            WEAK_SSLMODE,
        ),
        (
            "missing sslmode under required",
            None,
            format!("{remote}/fixture-source"),
            format!("{remote}/fixture-target"),
            MISSING_SSLMODE,
        ),
        (
            "remote host under local-only",
            Some("local-only"),
            format!("{remote}/fixture-source?sslmode=require"),
            format!("{remote}/fixture-target?sslmode=require"),
            REMOTE_REFUSED,
        ),
        (
            "unsupported sslmode under local-only",
            Some("local-only"),
            "postgres://fixture-user:fixture-secret@fixture-host/fixture-source?sslmode=fixture-mode"
                .to_owned(),
            "postgres://fixture-user:fixture-secret@fixture-host/fixture-target?sslmode=fixture-mode"
                .to_owned(),
            UNSUPPORTED_SSLMODE,
        ),
        (
            "invalid TWO_DATABASE_TLS setting",
            Some("fixture-invalid-setting"),
            format!("{remote}/fixture-source?sslmode=disable"),
            format!("{remote}/fixture-target?sslmode=disable"),
            INVALID_SETTING,
        ),
    ];
    for (case, tls, source, target, expected) in cases {
        let label = format!("{binary} {case}");
        let mut command = match binary {
            "legacy_copy" => legacy_copy(&source, &target, tls),
            _ => legacy_verify(&source, &target, tls),
        };
        let output = run_bounded(&mut command, &label);
        assert_tls_refusal(&output, expected, &label);
    }
}

#[test]
fn legacy_copy_tls_refusal_matrix_uses_fixed_strings() {
    tls_refusal_matrix("legacy_copy");
}

#[test]
fn legacy_verify_tls_refusal_matrix_uses_fixed_strings() {
    tls_refusal_matrix("legacy_verify");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FirstMessage {
    SslRequest,
    Startup,
}

/// Minimal Postgres-SSL-negotiation spy: records whether each connection
/// opens with an SSLRequest (code 80877103) or a plaintext StartupMessage
/// (protocol 196608), answers `N` (no SSL) to SSLRequests, then closes.
/// This proves the URL sslmode reaches the socket unchanged under
/// `local-only` — the enforced-mode half a process-level test can observe.
/// (The `required`-policy upgrade to `verify-full` happens in
/// `database_tls::apply`, whose effective mode is pinned by the core table
/// tests; a loopback spy cannot be reached under `required` at all, which
/// the refusal tests above prove instead.)
struct HandshakeSpy {
    port: u16,
    seen: Arc<Mutex<Vec<FirstMessage>>>,
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

impl HandshakeSpy {
    fn finish(self) -> Vec<FirstMessage> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.join().unwrap();
        let seen = self.seen.lock().unwrap();
        seen.clone()
    }
}

fn start_handshake_spy() -> HandshakeSpy {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let thread_seen = Arc::clone(&seen);
    let thread_stop = Arc::clone(&stop);
    let handle = std::thread::spawn(move || {
        while !thread_stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // Accepted streams can inherit the listener's nonblocking
                    // mode (e.g. on FreeBSD), where the read timeout alone
                    // does not stop `read_exact` returning `WouldBlock`.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut header = [0u8; 8];
                    if stream.read_exact(&mut header).is_ok() {
                        let code = u32::from_be_bytes(header[4..8].try_into().unwrap());
                        let kind = if code == 80877103 {
                            let _ = stream.write_all(b"N");
                            FirstMessage::SslRequest
                        } else {
                            FirstMessage::Startup
                        };
                        thread_seen.lock().unwrap().push(kind);
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    HandshakeSpy {
        port,
        seen,
        stop,
        handle,
    }
}

fn applies_url_sslmode_to_the_socket(binary: &str) {
    for (sslmode, expected) in [
        ("disable", FirstMessage::Startup),
        ("require", FirstMessage::SslRequest),
    ] {
        let label = format!("{binary} sslmode={sslmode}");
        let spy = start_handshake_spy();
        let source = spy_url(spy.port, "fixture-source", sslmode);
        let target = spy_url(spy.port, "fixture-target", sslmode);
        let mut command = match binary {
            "legacy_copy" => legacy_copy(&source, &target, Some("local-only")),
            _ => legacy_verify(&source, &target, Some("local-only")),
        };
        // The spy is not a Postgres server, so the run must fail — but only
        // after speaking (or not speaking) TLS negotiation first.
        let output = run_bounded(&mut command, &label);
        let seen = spy.finish();
        assert!(
            !output.status.success(),
            "{label}: run unexpectedly succeeded"
        );
        assert!(!seen.is_empty(), "{label}: binary never dialed the spy");
        assert!(
            seen.iter().all(|kind| *kind == expected),
            "{label}: expected every connection to open with {expected:?}, saw {seen:?}"
        );
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !combined.contains("fixture-secret"),
            "{label}: connection failure echoed the credential"
        );
    }
}

#[test]
fn legacy_copy_applies_url_sslmode_to_the_socket() {
    applies_url_sslmode_to_the_socket("legacy_copy");
}

#[test]
fn legacy_verify_applies_url_sslmode_to_the_socket() {
    applies_url_sslmode_to_the_socket("legacy_verify");
}

/// Green path that must stay green: `--plan` is documented as making no
/// connection, so TLS-policy fencing of connection construction must not
/// change it — even for URLs the policy would refuse to connect to.
#[test]
fn legacy_copy_plan_stays_offline_for_noncompliant_urls() {
    let output = Command::new(env!("CARGO_BIN_EXE_legacy_copy"))
        .env_clear()
        .args([
            "--plan",
            "--source-url=postgres://fixture-user:fixture-secret@ep-fixture-host.us-east-2.aws.neon.tech/fixture-source?sslmode=disable",
            "--target-url=postgres://fixture-user:fixture-secret@ep-fixture-host.us-east-2.aws.neon.tech/fixture-target?sslmode=disable",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["version"], 1);
    assert_eq!(plan["apply"], false);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("fixture"));
    assert!(output.stderr.is_empty());
}

// ---------------------------------------------------------------------------
// Disposable-Postgres acceptance (ignored): the same endpoints that verify
// green under an explicit `local-only` opt-in must refuse under the default
// policy, and a remote URL must refuse in each position. Scratch-database
// handling mirrors `legacy_verify_db.rs` exactly: only the fixed test
// endpoint behind `TWO_TEST_DATABASE_URL`, never an application URL.
// ---------------------------------------------------------------------------

fn test_database_url_allowed(url: &str, github_actions: bool) -> bool {
    url == "postgres://agent_test@agent-testdb:5432/agent_test"
        || (github_actions && url == "postgres://agent_test@127.0.0.1:5432/agent_test")
}

const SOURCE_FIXTURE: &str = r#"
CREATE TABLE legacy_members (
    id text PRIMARY KEY, enabled text NOT NULL, joined_at text NOT NULL,
    metadata text, display_name text
);
INSERT INTO legacy_members VALUES
    ('1','1','2026-09-30T03:00:00.123456+03:00','{"z":[true,{"b":2,"a":1}],"n":100.00}',NULL),
    ('2','false','2026-09-30T00:00:01Z','null','null');
"#;
const TARGET_FIXTURE: &str = r#"
CREATE TABLE next_members (
    member_id bigint PRIMARY KEY, enabled boolean NOT NULL, joined_at timestamptz NOT NULL,
    metadata jsonb, display_name text
);
INSERT INTO next_members VALUES
    (1,true,'2026-09-30T00:00:00.123456Z','{"n":1e2,"z":[true,{"a":1,"b":2}]}',NULL),
    (2,false,'2026-09-29T20:00:01-04:00','null','null');
"#;

#[tokio::test]
#[ignore = "requires agent-testdb or the CI service behind TWO_TEST_DATABASE_URL guard"]
async fn legacy_verify_policy_gates_the_real_test_endpoints() {
    use sqlx::{
        postgres::{PgConnectOptions, PgSslMode},
        Connection as _, PgConnection,
    };

    let url = std::env::var("TWO_TEST_DATABASE_URL").unwrap();
    let ci = std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
    assert!(
        test_database_url_allowed(&url, ci),
        "non-test database refused"
    );
    for key in ["PGOPTIONS", "PGSSLCERT", "PGSSLKEY", "PGSSLROOTCERT"] {
        assert!(
            std::env::var_os(key).is_none(),
            "inherited PG settings refused"
        );
    }
    let host = if url.contains("@127.0.0.1:") {
        "127.0.0.1"
    } else {
        "agent-testdb"
    };
    let options = PgConnectOptions::new_without_pgpass()
        .host(host)
        .port(5432)
        .username("agent_test")
        .password("")
        .database("agent_test")
        .ssl_mode(PgSslMode::Disable);
    let mut admin = PgConnection::connect_with(&options).await.unwrap();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let source_db = format!("tlsfence_{}_{}_source", std::process::id(), nonce);
    let target_db = format!("tlsfence_{}_{}_target", std::process::id(), nonce);
    for database in [&source_db, &target_db] {
        // Names contain only our constant prefix and process/time IDs.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {database}")))
            .execute(&mut admin)
            .await
            .unwrap();
    }

    // Scenario runs in a spawned task so cleanup below still happens when an
    // assertion panics (same pattern as `legacy_verify_db.rs`).
    let scenario = tokio::spawn({
        let options = options.clone();
        let host = host.to_owned();
        let source_db = source_db.clone();
        let target_db = target_db.clone();
        async move {
            let mut source_conn =
                PgConnection::connect_with(&options.clone().database(&source_db))
                    .await
                    .unwrap();
            let mut target_conn = PgConnection::connect_with(&options.database(&target_db))
                .await
                .unwrap();
            sqlx::raw_sql(SOURCE_FIXTURE)
                .execute(&mut source_conn)
                .await
                .unwrap();
            sqlx::raw_sql(TARGET_FIXTURE)
                .execute(&mut target_conn)
                .await
                .unwrap();
            source_conn.close().await.unwrap();
            target_conn.close().await.unwrap();

            let local_source = format!("postgres://agent_test@{host}:5432/{source_db}");
            let local_target = format!("postgres://agent_test@{host}:5432/{target_db}");
            let remote = "postgres://fixture-user:fixture-secret@ep-fixture-host.us-east-2.aws.neon.tech/fixture-db?sslmode=require";

            // Default policy (`required`): the real local endpoints refuse.
            let output = run_bounded(
                &mut legacy_verify(&local_source, &local_target, None),
                "verify default policy on test endpoints",
            );
            assert_tls_refusal(&output, LOCAL_REFUSED, "verify default policy");

            // Remote refuses under `local-only` in each position separately,
            // with the other side a real, reachable test database.
            let output = run_bounded(
                &mut legacy_verify(remote, &local_target, Some("local-only")),
                "verify remote source position",
            );
            assert_tls_refusal(&output, REMOTE_REFUSED, "verify remote source");
            let output = run_bounded(
                &mut legacy_verify(&local_source, remote, Some("local-only")),
                "verify remote target position",
            );
            assert_tls_refusal(&output, REMOTE_REFUSED, "verify remote target");

            // Green path: explicit `local-only` opt-in verifies the equal
            // fixtures successfully.
            let output = run_bounded(
                &mut legacy_verify(&local_source, &local_target, Some("local-only")),
                "verify local-only green path",
            );
            assert_eq!(
                output.status.code(),
                Some(0),
                "local-only green path failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(report["matches"], true);
        }
    })
    .await;

    // Exact two databases just created by this test, never shared app data.
    for database in [&source_db, &target_db] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {database} WITH (FORCE)"
        )))
        .execute(&mut admin)
        .await
        .unwrap();
    }
    admin.close().await.unwrap();
    scenario.unwrap();
}

#[test]
fn guard_rejects_real_endpoints_and_url_overrides() {
    assert!(test_database_url_allowed(
        "postgres://agent_test@agent-testdb:5432/agent_test",
        false
    ));
    for url in [
        "postgres://agent_test@127.0.0.1:5432/agent_test",
        "postgres://agent_test@agent-testdb:5432/production",
        "postgres://admin@agent-testdb:5432/agent_test",
        "postgres://agent_test@agent-testdb:5432/agent_test?host=production",
    ] {
        assert!(!test_database_url_allowed(url, false));
    }
}
