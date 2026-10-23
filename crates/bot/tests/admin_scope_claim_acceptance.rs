//! Failing acceptance for slice-1 scope-claim verification
//! (approved admin API spec sections 3.2 and 5.2).
//!
//! The RED side of the acceptance pair: every `valid_` / `fail_closed` /
//! `scoped_*` test below FAILS against the current binary (the receiver only
//! knows the v1 envelope, so an extended tag never verifies and appended
//! claim headers are ignored) and passes once the Builder lands the
//! canonical-string extension. The `v1_` / `tamper_matrix` tests pin what
//! must not change. Test files only; no `src` changes. The Builder builds on
//! this branch and never edits this file.
//!
//! What the tests prove, against a real booted binary with a disposable
//! migrated fixture database (never staging or production):
//!
//! 1. a request whose `x-two-scope` + `x-two-guild` claims are covered by the
//!    MAC passes authentication and reaches dispatch (observed as 403 for
//!    the unknown probe action: allowlisted, enabled, and executor checks
//!    all run after auth);
//! 2. any tamper of a covered claim — flipped scope or guild byte, dropped
//!    claim header, off-grammar value — refuses as the one indistinguishable
//!    401, burns no nonce, and writes no idempotency or audit row;
//! 3. a v1 signature carrying scope headers fails CLOSED (401): unsigned
//!    claims never verify as v1 and never feed a decision;
//! 4. `Idempotency-Key` stays outside the MAC: key variants verify
//!    identically under both schemes, and the golden canonicals carry no key
//!    line (`crates/core/tests/admin_scope_claim_acceptance.rs`).
//!
//! Pinned choices the Builder (and the CISO review) must honor or
//! renegotiate on the acceptance card: the eight-line canonical with the
//! `two-internal-action/scope-claim/v1` domain tag (goldens in
//! `crates/core/tests/fixtures/admin-scope-claim-signing.json`, recomputed
//! in-test below so the live signer cannot drift from them); the
//! `x-two-scope` (`read|moderate|admin`) + `x-two-guild` (snowflake)
//! envelope headers; scheme selection by claim presence with the `sha256=`
//! marker unchanged; fail-closed on unsigned claims (stricter than the
//! signing-v2 ignore rule, because a silently dropped scope would lie in
//! audit); the existing verify-then-burn-then-authorize order for the new
//! scheme.
//!
//! Discrimination note: the tamper matrix already 401s today (a scoped tag
//! never verifies as v1), so it passes for the wrong reason until the valid
//! test turns green. The pair jointly discriminates — intact verifies,
//! tampered refuses. The matrix's teeth come from the canonical: any claim
//! flip changes the signed bytes (proven at unit level by
//! `scope_claim_canonical_covers_claims_and_body_but_no_idempotency_key` in
//! the core suite), so a verifier that skips a claim line fails the matrix
//! while passing the valid test. The Builder re-proves this after GREEN by
//! flipping one golden byte and watching the matrix fail.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    process::Stdio,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, KeyInit as _, Mac as _};
use serde_json::Value;
use two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID;
use two_bot_core::internal_actions::body_hash;
use two_bot_testsupport::TestDatabase;

/// Unknown to the allowlist today and forever unimplementable under that
/// name: the 403 it draws proves authentication passed without executing or
/// claiming anything.
const PROBE_BODY: &str = "{\"action\":\"probe.no_such_verb_acceptance\"}";
const SCOPE_TAG: &str = "two-internal-action/scope-claim/v1";
const ACTIONS_PATH: &str = "/internal/actions";
const PROBE_SCOPE: &str = "moderate";
const PROBE_GUILD: &str = "123456789012345678";

/// Fixture caller identity installed via `TWO_INTERNAL_KEYS` (secret well over
/// the 32-byte minimum; fixture-only, never a real credential).
/// Low-entropy repeating characters only, so secret scanners stay quiet.
const FIXTURE_KEY_ID: &str = "fixture-scope-key";
fn fixture_secret() -> String {
    "s".repeat(40)
}

const FIXTURE_ADMIN_SERVICE_ID: &str = "fixture-admin-service";
fn fixture_admin_service_secret() -> String {
    "t".repeat(40)
}

/// The one message every auth failure returns: unknown key ids, bad
/// signatures, and tampered scope claims are one indistinguishable refusal.
const AUTH_FAILURE_MESSAGE: &str = "Signature verification failed";

/// The pinned eight-line canonical, over caller-supplied claim lines. Bound
/// to the fixture goldens by `live_signer_reproduces_the_goldens`, so a
/// drifted in-test signer fails loudly instead of weakening every test here.
fn scoped_canonical(timestamp: &str, nonce: &str, raw: &[u8], scope: &str, guild: &str) -> String {
    format!(
        "{SCOPE_TAG}\nPOST\n{ACTIONS_PATH}\n{timestamp}\n{nonce}\n{}\n{scope}\n{guild}",
        body_hash(raw)
    )
}

fn hmac_tag(secret: &str, canonical: &str) -> String {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC key");
    mac.update(canonical.as_bytes());
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn sign_v1(secret: &str, timestamp: &str, nonce: &str, raw: &[u8]) -> String {
    hmac_tag(
        secret,
        &format!(
            "POST\n{ACTIONS_PATH}\n{timestamp}\n{nonce}\n{}",
            body_hash(raw)
        ),
    )
}

fn sign_scoped(
    secret: &str,
    timestamp: &str,
    nonce: &str,
    raw: &[u8],
    scope: &str,
    guild: &str,
) -> String {
    hmac_tag(
        secret,
        &scoped_canonical(timestamp, nonce, raw, scope, guild),
    )
}

fn fresh_timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_secs()
        .to_string()
}

fn fresh_nonce() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

fn scope_fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../core/tests/fixtures/admin-scope-claim-signing.json"
    ))
    .expect("scope-claim vector fixture")
}

struct SignedRequest {
    timestamp: String,
    nonce: String,
    signature: String,
    scope: Option<String>,
    guild: Option<String>,
    extra_headers: Vec<(&'static str, String)>,
}

/// Mint a scope-claimed request whose tag covers `sign_scope`/`sign_guild`
/// but whose wire headers carry `send_scope`/`send_guild`: identical for the
/// intact case, diverged for tamper-after-sign.
fn mint(
    secret: &str,
    raw: &[u8],
    sign_scope: &str,
    sign_guild: &str,
    send_scope: Option<&str>,
    send_guild: Option<&str>,
) -> SignedRequest {
    let timestamp = fresh_timestamp();
    let nonce = fresh_nonce();
    SignedRequest {
        timestamp: timestamp.clone(),
        nonce: nonce.clone(),
        signature: sign_scoped(secret, &timestamp, &nonce, raw, sign_scope, sign_guild),
        scope: send_scope.map(str::to_owned),
        guild: send_guild.map(str::to_owned),
        extra_headers: Vec::new(),
    }
}

fn mint_v1(secret: &str, raw: &[u8]) -> SignedRequest {
    let timestamp = fresh_timestamp();
    let nonce = fresh_nonce();
    SignedRequest {
        signature: sign_v1(secret, &timestamp, &nonce, raw),
        timestamp,
        nonce,
        scope: None,
        guild: None,
        extra_headers: Vec::new(),
    }
}

struct Bot(tokio::process::Child);

impl Bot {
    async fn stop(mut self) {
        let _ = self.0.kill().await;
        let _ = self.0.wait().await;
    }
}

fn bootstrap() -> Option<String> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure scope-claim acceptance"
            );
            eprintln!("SKIP scope-claim acceptance: TWO_TEST_DATABASE_URL not set");
            None
        }
    }
}

async fn migrated() -> Option<TestDatabase> {
    Some(
        TestDatabase::create(&bootstrap()?, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated test database"),
    )
}

fn db_url(db: &TestDatabase) -> String {
    format!("postgres://agent_test:@agent-testdb:5432/{}", db.name())
}

fn reserve_port() -> u16 {
    let reserved = TcpListener::bind("127.0.0.1:0").expect("reserve test port");
    let port = reserved.local_addr().expect("reserved addr").port();
    assert_ne!(port, 8080);
    drop(reserved);
    port
}

/// Boot the real binary with the internal-action receiver enabled against a
/// disposable fixture database. Returns the child plus the receiver address.
/// stdout/stderr stay null: the gateway dials the fixture token forever, and
/// a piped log would stall the child once the buffer fills.
async fn spawn_bot(fixture_url: &str) -> (Bot, SocketAddr) {
    let main_port = reserve_port();
    let receiver_port = reserve_port();
    assert_ne!(main_port, receiver_port, "reserved ports must differ");
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_two-bot"));
    command
        .env_clear()
        .env("TWO_DATABASE_TLS", "local-only")
        .env("LISTEN_ADDR", format!("127.0.0.1:{main_port}"))
        .env("DISCORD_TOKEN", "fixture-token")
        .env("GUILD_ID", TWO_STAGING_GUILD_ID)
        .env("DATABASE_URL", fixture_url)
        .env("TWO_INTERNAL_ACTIONS", "1")
        .env("TWO_INTERNAL_ALLOW_EVENT_READ", "1")
        .env("TWO_INTERNAL_ALLOW_SETTINGS", "1")
        .env("TWO_INTERNAL_BIND", format!("127.0.0.1:{receiver_port}"))
        .env(
            "TWO_INTERNAL_KEYS",
            format!(
                "{FIXTURE_KEY_ID}:{},{FIXTURE_ADMIN_SERVICE_ID}:{}",
                fixture_secret(),
                fixture_admin_service_secret()
            ),
        )
        .env(
            "TWO_INTERNAL_CALLERS",
            format!("{FIXTURE_KEY_ID}:website-staging,{FIXTURE_ADMIN_SERVICE_ID}:website-staging"),
        )
        .env("TWO_INTERNAL_CHANNEL_KEYS", "ann:333333333333333333")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut bot = Bot(command.spawn().expect("start test bot"));
    let addr: SocketAddr = format!("127.0.0.1:{receiver_port}")
        .parse()
        .expect("receiver addr");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        // Readiness is the receiver itself answering: a bare POST draws the
        // 401 missing-header refusal, which proves the HMAC path is live.
        if let Ok(response) = raw_post(addr, PROBE_BODY, &[]) {
            if status_code(&response) == Some(401) {
                break;
            }
        }
        assert!(
            bot.0.try_wait().expect("polls the child").is_none(),
            "bot exited before serving the receiver port"
        );
        assert!(
            Instant::now() < deadline,
            "bot did not serve the receiver port"
        );
        // Bounded 20 ms backoff under the 20 s deadline: lets the child boot
        // without spinning a core per test. The file's single sleep.
        thread::sleep(Duration::from_millis(20));
    }
    (bot, addr)
}

fn raw_post(addr: SocketAddr, body: &str, headers: &[(&str, String)]) -> std::io::Result<String> {
    let timeout = Duration::from_secs(5);
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut request = format!(
        "POST {ACTIONS_PATH} HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes())?;
    // Read bytes (not `read_to_string`): a truncated or non-UTF-8 write still
    // yields a diagnostic body instead of an `InvalidData` transport error.
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn status_code(response: &str) -> Option<u16> {
    // Strict: only a complete `HTTP/<version> <code>` status line counts, so
    // a truncated write or a non-HTTP first line falls through to the retry
    // like the empty read does instead of returning a bogus code.
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
/// so an empty read or garbage bytes are visible instead of a bare `None`.
fn describe_response(response: &str) -> String {
    let preview: String = response.chars().take(300).collect();
    format!("len {} preview {preview:?}", response.len())
}

/// Bounded retry for the transient empty-read flake: the listener accepted
/// the connection and then closed it without a status line. Each attempt
/// mints fresh headers (fresh timestamp/nonce, so no replay) and only
/// unparseable/transport failures retry; a parseable status — even a wrong
/// one — returns immediately so a real regression still fails instead of
/// being retried away. No sleep here: the timing gate pins this file to its
/// single 20 ms spawn backoff, so retries are immediate.
fn post_with_fresh_headers(
    addr: SocketAddr,
    key_id: &str,
    make: impl Fn() -> (SignedRequest, Option<String>),
) -> Result<(String, SignedRequest), String> {
    post_body_with_fresh_headers(addr, key_id, PROBE_BODY, make)
}

fn post_body_with_fresh_headers(
    addr: SocketAddr,
    key_id: &str,
    raw: &str,
    make: impl Fn() -> (SignedRequest, Option<String>),
) -> Result<(String, SignedRequest), String> {
    const ATTEMPTS: usize = 3;
    let mut last_note = String::from("no attempts");
    for _ in 0..ATTEMPTS {
        let (signed, idempotency) = make();
        let mut headers = vec![
            ("x-two-key-id", key_id.to_owned()),
            ("x-two-timestamp", signed.timestamp.clone()),
            ("x-two-nonce", signed.nonce.clone()),
            ("x-two-signature", signed.signature.clone()),
        ];
        if let Some(scope) = &signed.scope {
            headers.push(("x-two-scope", scope.clone()));
        }
        if let Some(guild) = &signed.guild {
            headers.push(("x-two-guild", guild.clone()));
        }
        if let Some(key) = &idempotency {
            headers.push(("idempotency-key", key.clone()));
        }
        headers.extend(signed.extra_headers.clone());
        match raw_post(addr, raw, &headers) {
            Ok(response) if status_code(&response).is_some() => return Ok((response, signed)),
            Ok(response) => {
                last_note = describe_response(&response);
            }
            Err(error) => {
                last_note = format!("transport error: {error}");
            }
        }
    }
    Err(format!(
        "signed read got no parseable status after {ATTEMPTS} attempts: {last_note}"
    ))
}

fn body_json(response: &str) -> Value {
    let (_, body) = response.split_once("\r\n\r\n").expect("HTTP response body");
    serde_json::from_str(body).expect("receiver response must be JSON")
}

fn error_of(response: &str) -> Value {
    body_json(response)["error"].clone()
}

async fn nonce_burned(pool: &sqlx::PgPool, nonce: &str) -> bool {
    let hash = body_hash(nonce.as_bytes());
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM internal_nonces WHERE nonce_hash = $1")
        .bind(&hash)
        .fetch_one(pool)
        .await
        .expect("counts nonce burns")
        > 0
}

async fn terminal_rows(pool: &sqlx::PgPool) -> (i64, i64) {
    // Static literals only: sqlx 0.9 rejects a formatted query string.
    let intents = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM internal_idempotency")
        .fetch_one(pool)
        .await
        .expect("counts intent rows");
    let log = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM internal_action_log")
        .fetch_one(pool)
        .await
        .expect("counts audit rows");
    (intents, log)
}

#[test]
fn live_signer_reproduces_the_goldens() {
    // Binds the in-test signer above to the fixture goldens: any drift fails
    // here (not hidden inside an HMAC mismatch downstream), in both the Rust
    // verifier and — via the same bytes — the Worker signer the Builder adds.
    for (index, entry) in scope_fixture()["vectors"]
        .as_array()
        .expect("vector list")
        .iter()
        .enumerate()
    {
        let body = entry["body"].as_str().expect("body");
        let canonical = scoped_canonical(
            entry["timestamp"].as_str().expect("timestamp"),
            entry["nonce"].as_str().expect("nonce"),
            body.as_bytes(),
            entry["scope"].as_str().expect("scope"),
            entry["guild"].as_str().expect("guild"),
        );
        assert_eq!(
            canonical,
            entry["canonical"].as_str().expect("golden canonical"),
            "vector {index}: canonical recomputes byte-exact"
        );
        assert_eq!(
            hmac_tag(entry["secret"].as_str().expect("secret"), &canonical),
            entry["signature"].as_str().expect("golden signature"),
            "vector {index}: signature recomputes"
        );
    }
}

#[tokio::test]
async fn valid_scope_claim_passes_auth_and_reaches_dispatch() {
    // (1) RED until slice 1: an intact MAC-covered claim passes
    // authentication and reaches dispatch, observed as 403 for the unknown
    // probe action (every post-auth check runs before the allowlist). The
    // nonce burns exactly once (durable order for the new scheme) while no
    // idempotency or audit row appears (dispatch refuses pre-claim). Assumes
    // the spec section 3.1 order (flag authorize before the slice-0 scope
    // check), pinned by the slice-0 acceptance card: unknown actions refuse
    // before any registry read, so this stays green once slice 0 lands.
    let Some(db) = migrated().await else { return };
    let url = db_url(&db);
    let (bot, addr) = spawn_bot(&url).await;
    let secret = fixture_secret();
    let (response, signed) = post_with_fresh_headers(addr, FIXTURE_KEY_ID, || {
        (
            mint(
                &secret,
                PROBE_BODY.as_bytes(),
                PROBE_SCOPE,
                PROBE_GUILD,
                Some(PROBE_SCOPE),
                Some(PROBE_GUILD),
            ),
            None,
        )
    })
    .expect("intact scope-claim request");
    assert_eq!(
        status_code(&response),
        Some(403),
        "intact scope claim must pass auth to dispatch: {}",
        describe_response(&response)
    );
    assert_eq!(error_of(&response)["code"], "action_not_allowed");
    assert!(
        nonce_burned(db.pool(), &signed.nonce).await,
        "valid scope claim burns its nonce exactly once"
    );
    assert_eq!(
        terminal_rows(db.pool()).await,
        (0, 0),
        "refused requests claim nothing and audit nothing"
    );
    bot.stop().await;
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn scope_claim_tamper_matrix_refuses_indistinguishably() {
    // (2) Every tamper of a covered claim refuses as the one
    // indistinguishable 401: the refusal body equals the unknown-key
    // refusal, the tampered nonce never burns, and no claim or audit row
    // appears. Passes today for the wrong reason (a scoped tag never
    // verifies as v1); the valid test above proves the pair discriminates,
    // and the header records where the matrix's teeth come from.
    let Some(db) = migrated().await else { return };
    let url = db_url(&db);
    let (bot, addr) = spawn_bot(&url).await;
    let secret = fixture_secret();
    let (unknown_id, _) = post_with_fresh_headers(addr, "fixture-unknown-key", || {
        (mint_v1(&secret, PROBE_BODY.as_bytes()), None)
    })
    .expect("unknown-key refusal");
    assert_eq!(status_code(&unknown_id), Some(401));
    let reference_error = error_of(&unknown_id);
    assert_eq!(reference_error["code"], "unauthorized");
    assert_eq!(reference_error["message"], AUTH_FAILURE_MESSAGE);
    // A bad signature under the known id refuses identically: the tamper
    // cases below must match this same body, not merely the status.
    let (bad_signature, _) = post_with_fresh_headers(addr, FIXTURE_KEY_ID, || {
        let mut signed = mint_v1(&secret, PROBE_BODY.as_bytes());
        let last = signed.signature.pop().expect("signature byte");
        signed.signature.push(if last == '0' { '1' } else { '0' });
        (signed, None)
    })
    .expect("bad-signature refusal");
    assert_eq!(status_code(&bad_signature), Some(401));
    assert_eq!(error_of(&bad_signature), reference_error);

    let mut flipped_scope = PROBE_SCOPE.to_owned();
    flipped_scope.pop();
    flipped_scope.push('x');
    let mut flipped_guild = PROBE_GUILD.to_owned();
    flipped_guild.pop();
    flipped_guild.push('7');
    let cases: [(&str, Option<&str>, Option<&str>); 7] = [
        (
            "flipped scope byte",
            Some(flipped_scope.as_str()),
            Some(PROBE_GUILD),
        ),
        (
            "flipped guild digit",
            Some(PROBE_SCOPE),
            Some(flipped_guild.as_str()),
        ),
        ("dropped scope header", None, Some(PROBE_GUILD)),
        ("dropped guild header", Some(PROBE_SCOPE), None),
        ("uppercase scope", Some("MODERATE"), Some(PROBE_GUILD)),
        (
            "non-snowflake guild",
            Some(PROBE_SCOPE),
            Some("not-a-guild"),
        ),
        ("empty scope", Some(""), Some(PROBE_GUILD)),
    ];
    for (name, send_scope, send_guild) in cases {
        let (response, signed) = post_with_fresh_headers(addr, FIXTURE_KEY_ID, || {
            (
                mint(
                    &secret,
                    PROBE_BODY.as_bytes(),
                    PROBE_SCOPE,
                    PROBE_GUILD,
                    send_scope,
                    send_guild,
                ),
                None,
            )
        })
        .expect("tampered scope-claim request");
        assert_eq!(
            status_code(&response),
            Some(401),
            "{name} must refuse: {}",
            describe_response(&response)
        );
        assert_eq!(
            error_of(&response),
            reference_error,
            "{name} must be indistinguishable from the unknown-key refusal"
        );
        assert!(
            !nonce_burned(db.pool(), &signed.nonce).await,
            "{name} must burn no nonce"
        );
    }
    assert_eq!(
        terminal_rows(db.pool()).await,
        (0, 0),
        "refused requests claim nothing and audit nothing"
    );
    bot.stop().await;
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn v1_signature_with_appended_scope_headers_fails_closed() {
    // (3) RED until slice 1: a v1 tag next to scope headers is an unsigned
    // claim and must refuse (401), never verify as v1 with the claims
    // silently dropped. Fail-closed beats the signing-v2 ignore rule here:
    // a dropped scope would lie in audit about the authority acted under.
    let Some(db) = migrated().await else { return };
    let url = db_url(&db);
    let (bot, addr) = spawn_bot(&url).await;
    let secret = fixture_secret();
    let (response, signed) = post_with_fresh_headers(addr, FIXTURE_KEY_ID, || {
        let mut signed = mint_v1(&secret, PROBE_BODY.as_bytes());
        signed.scope = Some(PROBE_SCOPE.to_owned());
        signed.guild = Some(PROBE_GUILD.to_owned());
        (signed, None)
    })
    .expect("v1 signature with appended claims");
    assert_eq!(
        status_code(&response),
        Some(401),
        "unsigned scope claims must fail closed, never verify as v1: {}",
        describe_response(&response)
    );
    assert_eq!(error_of(&response)["code"], "unauthorized");
    assert_eq!(error_of(&response)["message"], AUTH_FAILURE_MESSAGE);
    assert!(
        !nonce_burned(db.pool(), &signed.nonce).await,
        "unsigned claims burn no nonce"
    );
    assert_eq!(
        terminal_rows(db.pool()).await,
        (0, 0),
        "refused requests claim nothing and audit nothing"
    );
    bot.stop().await;
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn v1_requests_and_idempotency_variants_still_verify_unchanged() {
    // (4a) Control: plain v1 requests keep verifying exactly as today, and
    // the Idempotency-Key value (two keys plus absent) changes nothing about
    // verification: all three draw the identical dispatch refusal. Green
    // before and after; the Builder must keep it green.
    let Some(db) = migrated().await else { return };
    let url = db_url(&db);
    let (bot, addr) = spawn_bot(&url).await;
    let secret = fixture_secret();
    let mut references: Vec<Value> = Vec::new();
    for key in [
        Some("scope-key-variant-one"),
        Some("scope-key-variant-two"),
        None,
    ] {
        let (response, signed) = post_with_fresh_headers(addr, FIXTURE_KEY_ID, || {
            (
                mint_v1(&secret, PROBE_BODY.as_bytes()),
                key.map(str::to_owned),
            )
        })
        .expect("v1 control request");
        assert_eq!(
            status_code(&response),
            Some(403),
            "v1 control must pass auth to dispatch (key {key:?}): {}",
            describe_response(&response)
        );
        references.push(error_of(&response));
        assert!(
            nonce_burned(db.pool(), &signed.nonce).await,
            "v1 control burns its nonce (key {key:?})"
        );
    }
    assert_eq!(references[0], references[1]);
    assert_eq!(references[0], references[2]);
    assert_eq!(
        terminal_rows(db.pool()).await,
        (0, 0),
        "dispatch refusals claim nothing and audit nothing"
    );
    bot.stop().await;
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn scoped_requests_ignore_idempotency_key_for_verify() {
    // (4b) RED until slice 1: the new scheme also leaves the key outside the
    // MAC — key variants verify identically (all reach dispatch). A Builder
    // who closes the v2 idempotency gap by mistake turns these 403s into
    // 401s and fails here.
    let Some(db) = migrated().await else { return };
    let url = db_url(&db);
    let (bot, addr) = spawn_bot(&url).await;
    let secret = fixture_secret();
    let mut references: Vec<Value> = Vec::new();
    for key in [
        Some("scope-key-variant-one"),
        Some("scope-key-variant-two"),
        None,
    ] {
        let (response, _) = post_with_fresh_headers(addr, FIXTURE_KEY_ID, || {
            (
                mint(
                    &secret,
                    PROBE_BODY.as_bytes(),
                    PROBE_SCOPE,
                    PROBE_GUILD,
                    Some(PROBE_SCOPE),
                    Some(PROBE_GUILD),
                ),
                key.map(str::to_owned),
            )
        })
        .expect("scoped key-variant request");
        assert_eq!(
            status_code(&response),
            Some(403),
            "scoped key variant must pass auth to dispatch (key {key:?}): {}",
            describe_response(&response)
        );
        references.push(error_of(&response));
    }
    assert_eq!(references[0], references[1]);
    assert_eq!(references[0], references[2]);
    assert_eq!(
        terminal_rows(db.pool()).await,
        (0, 0),
        "dispatch refusals claim nothing and audit nothing"
    );
    bot.stop().await;
    db.close().await.expect("drops fixture database");
}

// Bearer proof is distinct from service authentication. These are real receiver
// tests; the binding module's local scope checks are never registry authority.
// Public registry row identifier, not a bearer; the secret suffix is separate.
const PUBLIC_ADMIN_ID: &str = "adm_0123456789abcdef";
const TOKEN_SECRET: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn bearer() -> String {
    format!("{PUBLIC_ADMIN_ID}.{TOKEN_SECRET}")
}

async fn seed_read_token(pool: &sqlx::PgPool) {
    // The frozen slice-0 base lacks holder columns. Exercise that baseline and
    // the approved repaired schema without inventing a migration in tests.
    let has_holder: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns
         WHERE table_schema = 'public' AND table_name = 'admin_api_tokens'
         AND column_name = 'holder_kind')",
    )
    .fetch_one(pool)
    .await
    .expect("inspect fixture schema");
    let query = if has_holder {
        sqlx::query(
            "INSERT INTO admin_api_tokens
             (token_id, secret_hash, scopes, guild_ids, holder_id, holder_kind, issued_at, expires_at)
             VALUES ($1, $2, ARRAY['read'], ARRAY[123456789012345678]::bigint[],
                     'fixture-reader', 'human', now(), now() + interval '1 hour')",
        )
    } else {
        sqlx::query(
            "INSERT INTO admin_api_tokens
             (token_id, secret_hash, scopes, guild_ids, issued_at, expires_at)
             VALUES ($1, $2, ARRAY['read'], ARRAY[123456789012345678]::bigint[],
                     now(), now() + interval '1 hour')",
        )
    };
    query
        .bind(PUBLIC_ADMIN_ID)
        .bind(body_hash(TOKEN_SECRET.as_bytes()))
        .execute(pool)
        .await
        .expect("seed hash-only read token");
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM admin_api_tokens WHERE token_id = $1")
            .bind(PUBLIC_ADMIN_ID)
            .fetch_one(pool)
            .await
            .expect("confirm registry fixture");
    assert_eq!(count, 1, "refusal must test a real registered identity");
}

fn admin_body(action: &str, guild: &str, proof: Option<&str>) -> String {
    let mut value = serde_json::json!({
        "action": action, "guild_id": guild, "params": {},
        "token_id": PUBLIC_ADMIN_ID, "holder_kind": "human",
    });
    if let Some(proof) = proof {
        value["admin_bearer"] = Value::String(proof.to_owned());
    }
    value.to_string()
}

#[tokio::test]
async fn service_key_alone_cannot_impersonate_a_registered_bearer() {
    let Some(db) = migrated().await else { return };
    seed_read_token(db.pool()).await;
    let (bot, addr) = spawn_bot(&db_url(&db)).await;
    let wrong_secret = format!("{PUBLIC_ADMIN_ID}.{}", "z".repeat(40));
    for proof in [None, Some(wrong_secret.as_str())] {
        let raw = admin_body("event.read", PROBE_GUILD, proof);
        let (response, signed) = post_body_with_fresh_headers(addr, FIXTURE_KEY_ID, &raw, || {
            (
                mint(
                    &fixture_secret(),
                    raw.as_bytes(),
                    "read",
                    PROBE_GUILD,
                    Some("read"),
                    Some(PROBE_GUILD),
                ),
                None,
            )
        })
        .expect("signed bearer impersonation attempt");
        assert_eq!(
            status_code(&response),
            Some(401),
            "valid service MAC and registered token_id are not bearer proof: {}",
            describe_response(&response)
        );
        assert_eq!(error_of(&response)["code"], "unauthorized");
        assert!(
            nonce_burned(db.pool(), &signed.nonce).await,
            "service MAC verifies and burns nonce before registry refusal"
        );
        assert!(
            !response.contains(TOKEN_SECRET),
            "auth failures never echo token secrets"
        );
        assert!(
            !response.contains(&wrong_secret),
            "auth failures never echo presented proof"
        );
    }
    assert_eq!(
        terminal_rows(db.pool()).await,
        (0, 0),
        "no execution intent or receipt"
    );
    bot.stop().await;
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn freshly_signed_claims_cannot_widen_registry_scope_or_guild() {
    let Some(db) = migrated().await else { return };
    seed_read_token(db.pool()).await;
    let (bot, addr) = spawn_bot(&db_url(&db)).await;
    let proof = bearer();
    let cases = [
        ("widened scope", "event.read", "admin", PROBE_GUILD),
        ("widened guild", "event.read", "read", "987654321098765432"),
        (
            "ungranted write",
            "moderation.warn",
            "moderate",
            PROBE_GUILD,
        ),
    ];
    for (label, action, scope, guild) in cases {
        let raw = admin_body(action, guild, Some(&proof));
        let (response, signed) = post_body_with_fresh_headers(addr, FIXTURE_KEY_ID, &raw, || {
            (
                mint(
                    &fixture_secret(),
                    raw.as_bytes(),
                    scope,
                    guild,
                    Some(scope),
                    Some(guild),
                ),
                None,
            )
        })
        .expect("freshly signed authority-widening attempt");
        assert_eq!(
            status_code(&response),
            Some(403),
            "{label}: genuine bearer plus forged service claim must refuse: {}",
            describe_response(&response)
        );
        assert!(
            nonce_burned(db.pool(), &signed.nonce).await,
            "{label}: valid MAC reached authorization"
        );
        assert!(
            !response.contains(TOKEN_SECRET),
            "{label}: secret is transient auth input only"
        );
    }
    assert_eq!(
        terminal_rows(db.pool()).await,
        (0, 0),
        "no execution intent or receipt"
    );
    bot.stop().await;
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn known_and_unknown_scoped_verbs_stay_closed_even_with_live_bearer_and_flags() {
    let Some(db) = migrated().await else { return };
    seed_read_token(db.pool()).await;
    let (bot, addr) = spawn_bot(&db_url(&db)).await;
    let proof = bearer();
    // event.read/settings.get flags are explicitly ON in this process. A
    // default-off flag, bogus verb alone, or invalid MAC cannot prove closure.
    for action in [
        "event.read",
        "settings.get",
        "probe.no_such_verb_acceptance",
    ] {
        let raw = admin_body(action, PROBE_GUILD, Some(&proof));
        let (response, signed) = post_body_with_fresh_headers(addr, FIXTURE_KEY_ID, &raw, || {
            (
                mint(
                    &fixture_secret(),
                    raw.as_bytes(),
                    "read",
                    PROBE_GUILD,
                    Some("read"),
                    Some(PROBE_GUILD),
                ),
                None,
            )
        })
        .expect("closed scoped call");
        assert_eq!(
            status_code(&response),
            Some(403),
            "{action}: authentication succeeds but no scoped verb executes: {}",
            describe_response(&response)
        );
        assert_eq!(
            error_of(&response)["code"],
            "action_not_allowed",
            "{action}: closed dispatch refusal"
        );
        assert!(
            nonce_burned(db.pool(), &signed.nonce).await,
            "{action}: valid scoped MAC burns nonce"
        );
        assert_eq!(
            terminal_rows(db.pool()).await,
            (0, 0),
            "{action}: never claims or executes"
        );
    }
    bot.stop().await;
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn dedicated_admin_service_key_cannot_downgrade_to_v1_without_ambient_classification() {
    // Removing an ambient admin-key-id setting must not turn a dedicated key
    // into a legacy website key. spawn_bot env_clear() installs BOTH keys and
    // enables event.read, but sets no ambient admin-key classification at all.
    // No bearer, token_id, or claim header marks this request as admin traffic:
    // only the receiver's reserved key-id suffix can stop the downgrade.
    let Some(db) = migrated().await else { return };
    let (bot, addr) = spawn_bot(&db_url(&db)).await;
    let raw = r#"{"action":"event.read","event_key":"ghost"}"#;
    let malformed = r#"{"action":"event.read"}"#;
    let service_secret = fixture_admin_service_secret();

    let (legacy, legacy_signed) = post_body_with_fresh_headers(addr, FIXTURE_KEY_ID, raw, || {
        (mint_v1(&fixture_secret(), raw.as_bytes()), None)
    })
    .expect("ordinary legacy event.read control");
    let (legacy_malformed, malformed_signed) =
        post_body_with_fresh_headers(addr, FIXTURE_KEY_ID, malformed, || {
            (mint_v1(&fixture_secret(), malformed.as_bytes()), None)
        })
        .expect("ordinary legacy event.read validation control");
    let (unknown, _) = post_body_with_fresh_headers(addr, "fixture-unknown-key", raw, || {
        (mint_v1(&service_secret, raw.as_bytes()), None)
    })
    .expect("unknown-key reference refusal");
    let (bad_signature, _) = post_body_with_fresh_headers(addr, FIXTURE_KEY_ID, raw, || {
        (mint_v1(&service_secret, raw.as_bytes()), None)
    })
    .expect("known-key bad-signature reference refusal");
    let (downgrade, downgrade_signed) =
        post_body_with_fresh_headers(addr, FIXTURE_ADMIN_SERVICE_ID, raw, || {
            (mint_v1(&service_secret, raw.as_bytes()), None)
        })
        .expect("dedicated service key with valid legacy signature and no bearer");
    let (scoped, scoped_signed) = post_with_fresh_headers(addr, FIXTURE_ADMIN_SERVICE_ID, || {
        (
            mint(
                &service_secret,
                PROBE_BODY.as_bytes(),
                PROBE_SCOPE,
                PROBE_GUILD,
                Some(PROBE_SCOPE),
                Some(PROBE_GUILD),
            ),
            None,
        )
    })
    .expect("dedicated key's positive scoped authentication control");

    // Capture durable evidence and clean up before assertions, including RED.
    let legacy_burned = nonce_burned(db.pool(), &legacy_signed.nonce).await;
    let malformed_burned = nonce_burned(db.pool(), &malformed_signed.nonce).await;
    let downgrade_burned = nonce_burned(db.pool(), &downgrade_signed.nonce).await;
    let scoped_burned = nonce_burned(db.pool(), &scoped_signed.nonce).await;
    let rows = terminal_rows(db.pool()).await;
    bot.stop().await;
    db.close().await.expect("drops fixture database");

    assert_eq!(
        status_code(&legacy),
        Some(403),
        "ordinary v1 still reaches the existing unmapped-event refusal: {}",
        describe_response(&legacy)
    );
    assert_eq!(error_of(&legacy)["code"], "action_not_allowed");
    assert!(
        legacy_burned,
        "ordinary v1 authenticates and burns its nonce"
    );
    assert_eq!(
        status_code(&legacy_malformed),
        Some(400),
        "enabled legacy event.read still reaches payload validation, not a blanket auth/flag refusal"
    );
    assert_eq!(error_of(&legacy_malformed)["code"], "malformed");
    assert!(
        malformed_burned,
        "legacy validation occurs after nonce burn"
    );
    assert_eq!(status_code(&unknown), Some(401));
    assert_eq!(status_code(&bad_signature), Some(401));
    let reference_error = error_of(&unknown);
    assert_eq!(reference_error["code"], "unauthorized");
    assert_eq!(reference_error["message"], AUTH_FAILURE_MESSAGE);
    assert_eq!(error_of(&bad_signature), reference_error);

    assert_eq!(
        status_code(&downgrade),
        Some(401),
        "dedicated suffix refuses valid v1 without relying on any ambient admin-key setting: {}",
        describe_response(&downgrade)
    );
    assert_eq!(
        error_of(&downgrade),
        reference_error,
        "downgrade is indistinguishable from unknown key and invalid MAC"
    );
    assert!(
        !downgrade_burned,
        "dedicated-key v1 refuses before nonce burn"
    );
    assert!(
        body_json(&downgrade).get("result").is_none(),
        "no read result escapes"
    );
    assert_eq!(rows, (0, 0), "no execution intent or audit/effect receipt");
    assert_eq!(
        status_code(&scoped),
        Some(403),
        "dedicated key must be installed and usable for scoped authentication, not silently omitted"
    );
    assert_eq!(error_of(&scoped)["code"], "action_not_allowed");
    assert!(
        scoped_burned,
        "valid scoped service MAC reaches closed dispatch"
    );
}

#[tokio::test]
async fn legacy_v1_with_admin_bearer_cannot_bypass_the_scoped_closed_surface() {
    let Some(db) = migrated().await else { return };
    seed_read_token(db.pool()).await;
    let (bot, addr) = spawn_bot(&db_url(&db)).await;
    let proof = bearer();
    let raw = admin_body("event.read", PROBE_GUILD, Some(&proof));
    let (response, _) = post_body_with_fresh_headers(addr, FIXTURE_KEY_ID, &raw, || {
        (mint_v1(&fixture_secret(), raw.as_bytes()), None)
    })
    .expect("legacy admin bearer call");
    assert_eq!(
        status_code(&response),
        Some(401),
        "v1 is not a downgrade escape for admin bearer requests: {}",
        describe_response(&response)
    );
    assert_eq!(error_of(&response)["code"], "unauthorized");
    assert_eq!(
        terminal_rows(db.pool()).await,
        (0, 0),
        "no execution through legacy envelope"
    );
    bot.stop().await;
    db.close().await.expect("drops fixture database");
}

#[tokio::test]
async fn malformed_self_signed_and_duplicate_claim_headers_fail_before_nonce_burn() {
    let Some(db) = migrated().await else { return };
    let (bot, addr) = spawn_bot(&db_url(&db)).await;
    for (scope, guild) in [
        ("READ", PROBE_GUILD),
        ("read,admin", PROBE_GUILD),
        ("read", "0"),
        ("read", "18446744073709551616"),
        ("read", "-1"),
    ] {
        let (response, signed) = post_with_fresh_headers(addr, FIXTURE_KEY_ID, || {
            (
                mint(
                    &fixture_secret(),
                    PROBE_BODY.as_bytes(),
                    scope,
                    guild,
                    Some(scope),
                    Some(guild),
                ),
                None,
            )
        })
        .expect("malformed self-signed claims");
        assert_eq!(
            status_code(&response),
            Some(401),
            "malformed signed claim refuses"
        );
        assert!(
            !nonce_burned(db.pool(), &signed.nonce).await,
            "invalid grammar burns nothing"
        );
    }
    for (header, value) in [("x-two-scope", PROBE_SCOPE), ("x-two-guild", PROBE_GUILD)] {
        let (response, signed) = post_with_fresh_headers(addr, FIXTURE_KEY_ID, || {
            let mut signed = mint(
                &fixture_secret(),
                PROBE_BODY.as_bytes(),
                PROBE_SCOPE,
                PROBE_GUILD,
                Some(PROBE_SCOPE),
                Some(PROBE_GUILD),
            );
            signed.extra_headers.push((header, value.to_owned()));
            (signed, None)
        })
        .expect("duplicate scope/guild headers");
        assert_eq!(
            status_code(&response),
            Some(401),
            "duplicate {header} refuses even when identical"
        );
        assert!(
            !nonce_burned(db.pool(), &signed.nonce).await,
            "duplicate header burns nothing"
        );
    }
    assert_eq!(terminal_rows(db.pool()).await, (0, 0));
    bot.stop().await;
    db.close().await.expect("drops fixture database");
}
