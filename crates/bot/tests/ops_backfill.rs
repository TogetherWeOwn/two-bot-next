//! Failing acceptance for the backfill `/ops/backfill/progress` progress and ETA route.
//!
//! Paired failing-tests card for the `/ops/backfill/progress` build
//! ([TOG-22953](/TOG/issues/TOG-22953), slice 8 of TOG-22852 AC3). Every test
//! below FAILS against the current binary (the route does not exist, so the
//! listener answers 404) and passes once the builder lands the route. Test
//! files only; no `src` changes.
//!
//! What the tests prove, against fakes (never staging or production):
//!
//! 1. an authorized read reports per-family progress and ETA derived from the
//!    cursor store (the nine TOG-22852 families, each with a `progress`
//!    fraction and an `eta_seconds` estimate);
//! 2. the route keeps the existing listener's auth surface: the same
//!    `TWO_INTERNAL_KEYS` ring, the same `x-two-*` signed headers, and the
//!    same indistinguishable 401 refusal (`Signature verification failed`)
//!    for missing and wrong credentials alike. The exact surface stays under
//!    the CISO gate; these tests pin its properties, not a bypass;
//! 3. exposition uses fixed cardinality: family keys come from the bounded
//!    nine-family vocabulary, never cursors or request-unique values, and
//!    repeat reads expose identical key sets;
//! 4. bodies carry ids and counts only: no free-text string (message bodies
//!    and custom-status text always contain spaces; ids, counts, timestamps
//!    and enum tokens never do) and no `message`/`body`/`text`-shaped keys.
//!
//! Pinned choices the builder (and the CISO review) must honor or renegotiate
//! on this card: `GET /ops/backfill/progress` with a JSON `{"families": {...}}` body
//! (Prometheus already has `/metrics`; this read joins `/readyz`-style JSON
//! on the same listener), and a path-bound canonical string
//! `GET\n/ops/backfill/progress\n{ts}\n{nonce}\n{sha256_hex(body)}` so a signature cut
//! for `/internal/actions` never authorizes a backfill read.
//!
//! Fakes only: the binary boots parked (`DISCORD_TOKEN=INVALID`, an
//! unconnectable synthetic `DATABASE_URL`, no `TWO_INTERNAL_ACTIONS`
//! receiver) and every signature is minted in-test from a fixture
//! `TWO_INTERNAL_KEYS` entry. No database is touched, so no CI `check.yml`
//! step and no `nightly.yml` `--skip` line is required for this target.

use std::{
    cell::Cell,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, KeyInit as _, Mac as _};
use serde_json::Value;
use sha2::Digest as _;

/// Route under test. Absent until the builder lands it (today: 404).
/// Moved from `/ops/backfill` to `/ops/backfill/progress`: the open
/// `/ops/backfill` table roll-up from #918 already owns that path, and
/// registering both panics the listener (`Overlapping method route`).
const BACKFILL_PATH: &str = "/ops/backfill/progress";

/// The nine backfill families from the TOG-22852 plan (CEO decision); also the
/// bounded exposition vocabulary. Mirrors
/// `crates/store/tests/backfill_cursors.rs`.
const BACKFILL_FAMILIES: [&str; 9] = [
    "audit",
    "members",
    "history",
    "event_users",
    "reactions",
    "pins",
    "bans",
    "invites",
    "onboarding",
];

/// Fixture caller identity installed via `TWO_INTERNAL_KEYS` (secret well over
/// the 32-byte minimum; fixture-only, never a real credential).
/// Low-entropy repeating characters only, so secret scanners stay quiet.
const FIXTURE_KEY_ID: &str = "fixture-backfill-key";
fn fixture_secret() -> String {
    format!("{}-secret", "x".repeat(40))
}

/// The one message every auth failure returns on the existing surface: an
/// unknown key id and a wrong signature are one indistinguishable refusal.
const AUTH_FAILURE_MESSAGE: &str = "Signature verification failed";

/// Keys that must never appear at any depth: per-user content travels in
/// message-shaped fields, never in ids/counts/timestamps.
const FORBIDDEN_KEYS: [&str; 7] = [
    "message",
    "body",
    "text",
    "content",
    "status_text",
    "custom_status",
    "subject",
];

struct Bot(Child);

impl Drop for Bot {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_bot() -> (Bot, SocketAddr) {
    // Reserve a non-default local port; no deployed service is contacted.
    let reserved = TcpListener::bind("127.0.0.1:0").expect("reserve test port");
    let addr = reserved.local_addr().unwrap();
    assert_ne!(addr.port(), 8080);
    drop(reserved);

    // Parked boot: OMIT DATABASE_URL so gateway prerequisites fail and the
    // store stays None (empty-baseline 200). A synthetic non-postgres URL
    // with all three prerequisites present exits 1 via database_init
    // (see startup.rs configured_*_exits_nonzero), it never parks.
    let mut bot = Bot(Command::new(env!("CARGO_BIN_EXE_two-bot"))
        .env_clear()
        .env("LISTEN_ADDR", format!("127.0.0.1:{}", addr.port()))
        .env("DISCORD_TOKEN", "INVALID")
        .env("GUILD_ID", "123")
        .env(
            "TWO_INTERNAL_KEYS",
            format!("{}:{}", FIXTURE_KEY_ID, fixture_secret()),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start test bot"));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(response) = raw_get(addr, "/health", &[]) {
            if status_code(&response) == Some(200) {
                break;
            }
        }
        assert!(
            bot.0.try_wait().unwrap().is_none(),
            "bot exited before serving the configured port"
        );
        assert!(
            Instant::now() < deadline,
            "bot did not serve health on the configured port"
        );
        // Bounded 20 ms backoff under the 15 s deadline (~50 health
        // polls/s): lets the child boot without spinning a core per test.
        thread::sleep(Duration::from_millis(20));
    }
    (bot, addr)
}

fn raw_get(addr: SocketAddr, path: &str, headers: &[(&str, String)]) -> std::io::Result<String> {
    let timeout = Duration::from_secs(2);
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut request = format!("GET {path} HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes())?;
    // Read bytes (not `read_to_string`): a truncated or non-UTF-8 write still
    // yields a diagnostic body instead of an `InvalidData` transport error.
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn status_code(response: &str) -> Option<u16> {
    // Strict: only a complete `HTTP/<version> <code>` status line counts, so
    // a truncated write (e.g. `HTTP/1.1 2`) or a non-HTTP first line falls
    // through to the retry like the empty read does instead of returning a
    // bogus code.
    let line = response.lines().next()?;
    let mut parts = line.split_whitespace();
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    let code: u16 = parts.next()?.parse().ok()?;
    if !(100..=599).contains(&code) {
        return None;
    }
    Some(code)
}

/// One-line diagnostic for failure context: length plus an escaped preview
/// so an empty read (closed without a status line) or garbage bytes are
/// visible in the failure output instead of a bare `None`.
fn describe_response(response: &str) -> String {
    let preview: String = response.chars().take(300).collect();
    format!("len {} preview {preview:?}", response.len())
}

/// Bounded retry for the transient empty-read flake: the listener accepted
/// the connection and then closed it without a status line, so `status_code`
/// is `None`. Each attempt mints fresh headers (fresh timestamp/nonce, so no
/// replay) and only unparseable/transport failures retry; a parseable status
/// — even a wrong one — returns immediately so a real auth regression still
/// fails instead of being retried away. No sleep here: the timing gate pins
/// this file to its single 20 ms spawn backoff, so retries are immediate.
fn get_with_fresh_headers(
    addr: SocketAddr,
    path: &str,
    make_headers: impl Fn() -> Vec<(&'static str, String)>,
) -> Result<String, String> {
    const ATTEMPTS: usize = 3;
    let mut last_note = String::from("no attempts");
    for _ in 0..ATTEMPTS {
        let headers = make_headers();
        match raw_get(addr, path, &headers) {
            Ok(response) if status_code(&response).is_some() => return Ok(response),
            Ok(response) => {
                last_note = describe_response(&response);
            }
            Err(error) => {
                last_note = format!("transport error: {error}");
            }
        }
    }
    Err(format!(
        "backfill read got no parseable status after {ATTEMPTS} attempts: {last_note}"
    ))
}

fn body_json(response: &str) -> Value {
    let (_, body) = response.split_once("\r\n\r\n").expect("HTTP response body");
    serde_json::from_str(body).expect("backfill response must be JSON")
}

/// Mint the existing surface's signature, bound to the backfill path: the
/// same `x-two-*` headers and `sha256=` HMAC envelope as `/internal/actions`,
/// over `GET\n/ops/backfill/progress\n{ts}\n{nonce}\n{sha256_hex(body)}`.
fn backfill_auth_headers(key_id: &str, secret: &str) -> Vec<(&'static str, String)> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_secs()
        .to_string();
    let nonce = hex::encode(rand::random::<[u8; 16]>());
    let body_hash = hex::encode(sha2::Sha256::digest(b""));
    let canonical = format!("GET\n{BACKFILL_PATH}\n{timestamp}\n{nonce}\n{body_hash}");
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC key");
    mac.update(canonical.as_bytes());
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    vec![
        ("Content-Type", "application/json".to_owned()),
        ("x-two-key-id", key_id.to_owned()),
        ("x-two-timestamp", timestamp),
        ("x-two-nonce", nonce),
        ("x-two-signature", signature),
    ]
}

fn signed_get(addr: SocketAddr) -> String {
    get_with_fresh_headers(addr, BACKFILL_PATH, || {
        backfill_auth_headers(FIXTURE_KEY_ID, &fixture_secret())
    })
    .expect("signed backfill read")
}

fn families_of(body: &Value) -> &serde_json::Map<String, Value> {
    body.get("families")
        .and_then(Value::as_object)
        .expect("backfill body must carry a per-family progress map")
}

/// Every object key in the body, at any depth. Labels are vocabulary, never
/// values: a cursor or request-unique token surfacing as a key breaks the
/// fixed-cardinality contract.
fn all_keys(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                out.push(key.clone());
                all_keys(nested, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                all_keys(item, out);
            }
        }
        _ => {}
    }
}

fn all_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Object(map) => {
            for nested in map.values() {
                all_strings(nested, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                all_strings(item, out);
            }
        }
        _ => {}
    }
}

#[test]
fn signed_backfill_read_reports_per_family_progress_and_eta() {
    // (1): per-family progress + ETA derived from the cursor store. The parked
    // fixture holds no checkpoints, so every family reports the empty-store
    // baseline: a progress fraction in [0, 1], an ETA that is a number or
    // null, and a cursor that is an opaque id or null.
    let (_bot, addr) = spawn_bot();
    let response = signed_get(addr);
    assert_eq!(
        status_code(&response),
        Some(200),
        "authorized backfill read must succeed: {}",
        describe_response(&response)
    );
    let body = body_json(&response);
    let families = families_of(&body);
    let mut seen: Vec<&str> = families.keys().map(String::as_str).collect();
    seen.sort_unstable();
    let mut expected = BACKFILL_FAMILIES.to_vec();
    expected.sort_unstable();
    assert_eq!(seen, expected, "one progress entry per backfill family");
    for family in BACKFILL_FAMILIES {
        let entry = &families[family];
        let progress = entry
            .get("progress")
            .and_then(Value::as_f64)
            .expect("each family reports a numeric progress fraction");
        assert!(
            (0.0..=1.0).contains(&progress),
            "{family} progress {progress} must be a fraction"
        );
        match entry.get("eta_seconds") {
            None | Some(Value::Null) => {}
            Some(Value::Number(eta)) => {
                assert!(
                    eta.as_f64().is_some_and(|eta| eta >= 0.0),
                    "{family} ETA must be non-negative"
                );
            }
            Some(other) => panic!("{family} eta_seconds must be a number or null, got {other}"),
        }
        match entry.get("cursor") {
            None | Some(Value::Null) => {}
            Some(Value::String(cursor)) => {
                assert!(
                    !cursor.is_empty() && cursor.bytes().all(|b| b.is_ascii_digit()),
                    "{family} cursor must be an opaque id, got {cursor:?}"
                );
            }
            Some(other) => panic!("{family} cursor must be an id or null, got {other}"),
        }
    }
}

#[test]
fn unsigned_and_mistaken_backfill_callers_share_one_401_refusal() {
    // (2): the route keeps the existing listener's auth surface. A bare read
    // and a read signed under an unknown key id are one indistinguishable
    // 401, carrying the shared failure message and no key, cursor or
    // progress material.
    let (_bot, addr) = spawn_bot();
    let bare = get_with_fresh_headers(addr, BACKFILL_PATH, Vec::new).expect("bare backfill read");
    assert_eq!(
        status_code(&bare),
        Some(401),
        "credential-less backfill read must be refused: {}",
        describe_response(&bare)
    );
    let wrong = get_with_fresh_headers(addr, BACKFILL_PATH, || {
        backfill_auth_headers("fixture-unknown-key", &fixture_secret())
    })
    .expect("wrong-key backfill read");
    assert_eq!(
        status_code(&wrong),
        Some(401),
        "unknown-key backfill read must be refused: {}",
        describe_response(&wrong)
    );
    let bare_error = body_json(&bare)["error"].clone();
    let wrong_error = body_json(&wrong)["error"].clone();
    assert_eq!(
        bare_error, wrong_error,
        "missing and wrong credentials must be indistinguishable"
    );
    assert_eq!(bare_error["code"], "unauthorized");
    assert_eq!(bare_error["message"], AUTH_FAILURE_MESSAGE);
    for body in [body_json(&bare), body_json(&wrong)] {
        let text = body.to_string();
        assert!(
            !text.contains(FIXTURE_KEY_ID) && !text.contains("fixture-unknown-key"),
            "refusal must not echo key material: {text}"
        );
        assert!(
            body.get("families").is_none(),
            "refusal must not leak progress: {text}"
        );
    }
}

#[test]
fn backfill_exposition_keeps_fixed_cardinality() {
    // (3): fixed cardinality, no unbounded labels. Family keys stay inside
    // the nine-family vocabulary, every other key is vocabulary-shaped
    // (never a cursor or request-unique token), and repeat reads expose
    // identical key sets.
    let (_bot, addr) = spawn_bot();
    let first_raw = signed_get(addr);
    assert_eq!(
        status_code(&first_raw),
        Some(200),
        "authorized backfill read must succeed: {}",
        describe_response(&first_raw)
    );
    let second_raw = signed_get(addr);
    assert_eq!(
        status_code(&second_raw),
        Some(200),
        "authorized backfill read must succeed: {}",
        describe_response(&second_raw)
    );
    let first = body_json(&first_raw);
    let second = body_json(&second_raw);
    let mut first_keys = Vec::new();
    let mut second_keys = Vec::new();
    all_keys(&first, &mut first_keys);
    all_keys(&second, &mut second_keys);
    first_keys.sort();
    second_keys.sort();
    assert_eq!(
        first_keys, second_keys,
        "repeat reads must expose identical keys"
    );
    let families = families_of(&first);
    for key in families.keys() {
        assert!(
            BACKFILL_FAMILIES.contains(&key.as_str()),
            "family label {key:?} outside the bounded vocabulary"
        );
    }
    for key in &first_keys {
        assert!(
            !key.bytes().any(|b| b.is_ascii_digit()),
            "exposition key {key:?} carries value-shaped content"
        );
        assert!(
            key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "exposition key {key:?} must be fixed vocabulary"
        );
    }
}

#[test]
fn backfill_body_carries_ids_and_counts_only() {
    // (4): privacy fence. No per-user content beyond ids/counts: every string
    // in the body is an id, a timestamp, or a spaceless enum token (message
    // bodies and custom-status text always contain spaces), and no
    // message-shaped key appears at any depth.
    let (_bot, addr) = spawn_bot();
    let raw = signed_get(addr);
    assert_eq!(
        status_code(&raw),
        Some(200),
        "authorized backfill read must succeed: {}",
        describe_response(&raw)
    );
    let body = body_json(&raw);
    let mut keys = Vec::new();
    all_keys(&body, &mut keys);
    for key in &keys {
        assert!(
            !FORBIDDEN_KEYS.contains(&key.as_str()),
            "per-user field {key:?} must never be exposed"
        );
    }
    let mut strings = Vec::new();
    all_strings(&body, &mut strings);
    assert!(!strings.is_empty(), "body must carry id/count content");
    for text in &strings {
        assert!(
            !text.chars().any(|c| c.is_whitespace()),
            "free-text content {text:?} must never be exposed"
        );
    }
}

/// Serve one scripted payload per accepted connection: drain the request so
/// the client write succeeds, then write the bytes (possibly empty) and close
/// by drop. An empty payload reproduces the transient empty read (accept then
/// close without a status line). Blocking accepts only; no backoff here.
fn serve_scripted(listener: TcpListener, scripts: Vec<Vec<u8>>) {
    for script in scripts {
        let (mut conn, _) = listener.accept().expect("fake listener accept");
        let mut drain = [0u8; 4096];
        let _ = conn.read(&mut drain);
        if !script.is_empty() {
            conn.write_all(&script).expect("fake listener write");
        }
    }
}

#[test]
fn retry_recovers_from_empty_and_truncated_reads() {
    // Pins the flake fix: a dropped connection and a truncated status line
    // must both retry with fresh headers and return the next good response.
    // With `ATTEMPTS` reverted to 1 this goes red (the retry never runs).
    let good = b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_vec();
    for first in [Vec::new(), b"HTTP/1.1 2".to_vec()] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake listener");
        let addr = listener.local_addr().expect("fake listener addr");
        let good_next = good.clone();
        let handle = thread::spawn(move || serve_scripted(listener, vec![first, good_next]));
        let attempts = Cell::new(0u32);
        let response = get_with_fresh_headers(addr, BACKFILL_PATH, || {
            attempts.set(attempts.get() + 1);
            Vec::new()
        })
        .expect("retry must recover from an unparseable read");
        assert_eq!(
            status_code(&response),
            Some(200),
            "retry must return the good response: {}",
            describe_response(&response)
        );
        assert_eq!(
            attempts.get(),
            2,
            "empty/truncated read must cost exactly one retry"
        );
        handle.join().expect("fake server thread");
    }
}

#[test]
fn retry_returns_parseable_status_without_extra_attempt() {
    // Pins the no-retry guard: a parseable denial must return immediately so
    // a real auth regression fails instead of being retried away.
    let denied = b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n".to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake listener");
    let addr = listener.local_addr().expect("fake listener addr");
    // Serve exactly one response, then close: a parseable status must return
    // after one attempt, so the server thread must exit without a second
    // accept (serving more would hang `join`). If the client ever retried, the
    // refused connection would surface as a transport error and fail the test.
    let handle = thread::spawn(move || serve_scripted(listener, vec![denied]));
    let attempts = Cell::new(0u32);
    let response = get_with_fresh_headers(addr, BACKFILL_PATH, || {
        attempts.set(attempts.get() + 1);
        Vec::new()
    })
    .expect("parseable denial must return");
    assert_eq!(
        status_code(&response),
        Some(401),
        "parseable denial must pass through: {}",
        describe_response(&response)
    );
    assert_eq!(attempts.get(), 1, "parseable status must not retry");
    handle.join().expect("fake server thread");
}
