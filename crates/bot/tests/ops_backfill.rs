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
        // Deterministic sync: re-poll on scheduler wakeup under the deadline
        // instead of a wall-clock backoff sleep.
        thread::yield_now();
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
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

fn status_code(response: &str) -> Option<u16> {
    response
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
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
    let headers = backfill_auth_headers(FIXTURE_KEY_ID, &fixture_secret());
    let refs: Vec<(&str, String)> = headers;
    raw_get(addr, BACKFILL_PATH, &refs).expect("signed backfill read")
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
        "authorized backfill read must succeed"
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
    let bare = raw_get(addr, BACKFILL_PATH, &[]).expect("bare backfill read");
    assert_eq!(
        status_code(&bare),
        Some(401),
        "credential-less backfill read must be refused"
    );
    let wrong_headers = backfill_auth_headers("fixture-unknown-key", &fixture_secret());
    let wrong = raw_get(addr, BACKFILL_PATH, &wrong_headers).expect("wrong-key backfill read");
    assert_eq!(
        status_code(&wrong),
        Some(401),
        "unknown-key backfill read must be refused"
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
        "authorized backfill read must succeed"
    );
    let second_raw = signed_get(addr);
    assert_eq!(
        status_code(&second_raw),
        Some(200),
        "authorized backfill read must succeed"
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
        "authorized backfill read must succeed"
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
