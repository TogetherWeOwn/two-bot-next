//! Receiver regression for the registry's finite, <=7_776_000 elapsed-second
//! lifetime contract. Calendar-day arithmetic can incorrectly turn a valid
//! bearer into 401 across spring DST; authenticated scoped calls must still
//! stop at the closed 403 surface, never execute.
//!
//! The fixture installs a POSIX timezone ONLY on its owned disposable database.
//! Its spring transition is the first Sunday of the current UTC month, keeping
//! the transition inside a live 90-day token in every season/year. New receiver
//! connections inherit that database default; no bootstrap/role/global setting
//! or production implementation is changed.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    process::Stdio,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, KeyInit as _, Mac as _};
use serde_json::Value;
use two_bot_core::{backup::guild_config::TWO_STAGING_GUILD_ID, internal_actions::body_hash};
use two_bot_testsupport::TestDatabase;

const SERVICE_ID: &str = "elapsed-fixture-admin-service";
const GUILD: &str = "123456789012345678";
const PATH: &str = "/internal/actions";

fn secret() -> String {
    "s".repeat(40)
}

fn token_secret() -> String {
    "a".repeat(40)
}

async fn fixture() -> TestDatabase {
    let bootstrap = std::env::var("TWO_TEST_DATABASE_URL").expect(
        "configure the authorized disposable test bootstrap for elapsed-lifetime acceptance",
    );
    TestDatabase::create(&bootstrap, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated disposable database")
}

async fn install_spring_timezone(db: &TestDatabase) -> String {
    let month: i32 =
        sqlx::query_scalar("SELECT extract(month FROM now() AT TIME ZONE 'UTC')::integer")
            .fetch_one(db.pool())
            .await
            .expect("read fixture UTC month");
    let autumn = (month + 5) % 12 + 1;
    let zone = format!("STD0DST,M{month}.1.0,M{autumn}.1.0");
    // TestDatabase owns this generated, guarded identifier. The zone contains
    // only fixed POSIX grammar and server-derived integer months, not input.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER DATABASE \"{}\" SET timezone TO '{zone}'",
        db.name()
    )))
    .execute(db.pool())
    .await
    .expect("set only owned disposable database timezone");
    zone
}

struct Bot(tokio::process::Child);

impl Bot {
    async fn stop(mut self) {
        self.0.kill().await.expect("stop receiver fixture");
        self.0.wait().await.expect("reap receiver fixture");
    }
}

async fn spawn_bot(db: &TestDatabase) -> (Bot, SocketAddr) {
    // Keep both reservations live until immediately before spawning, so the
    // two binds cannot return the same port.
    let main = TcpListener::bind("127.0.0.1:0").expect("reserve health port");
    let receiver = TcpListener::bind("127.0.0.1:0").expect("reserve receiver port");
    let main_addr = main.local_addr().expect("health address");
    let addr = receiver.local_addr().expect("receiver address");
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_two-bot"));
    command
        .env_clear()
        .env("TWO_DATABASE_TLS", "local-only")
        .env(
            "DATABASE_URL",
            format!("postgres://agent_test:@agent-testdb:5432/{}", db.name()),
        )
        .env("LISTEN_ADDR", main_addr.to_string())
        .env("DISCORD_TOKEN", "fixture-token")
        .env("GUILD_ID", TWO_STAGING_GUILD_ID)
        .env("TWO_INTERNAL_ACTIONS", "1")
        .env("TWO_INTERNAL_ALLOW_EVENT_READ", "1")
        .env("TWO_INTERNAL_ALLOW_SETTINGS", "1")
        .env("TWO_INTERNAL_BIND", addr.to_string())
        .env("TWO_INTERNAL_KEYS", format!("{SERVICE_ID}:{}", secret()))
        .env(
            "TWO_INTERNAL_CALLERS",
            format!("{SERVICE_ID}:website-staging"),
        )
        .env("TWO_INTERNAL_CHANNEL_KEYS", "ann:333333333333333333")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    drop((main, receiver));
    let mut bot = Bot(command.spawn().expect("spawn actual receiver binary"));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(response) = post(addr, "{}", &[]) {
            if status(&response) == Some(401) {
                return (bot, addr);
            }
        }
        assert!(
            bot.0.try_wait().expect("child status").is_none(),
            "receiver exited during setup"
        );
        assert!(Instant::now() < deadline, "receiver startup deadline");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn post(addr: SocketAddr, body: &str, headers: &[(&str, String)]) -> std::io::Result<String> {
    let timeout = Duration::from_secs(5);
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "POST {PATH} HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    )?;
    for (name, value) in headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(stream, "\r\n{body}")?;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn status(response: &str) -> Option<u16> {
    let mut parts = response.lines().next()?.split_whitespace();
    if !parts.next()?.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse().ok()
}

fn signed_post(addr: SocketAddr, id: &str, action: &str) -> (String, String) {
    let body = serde_json::json!({
        "action": action, "guild_id": GUILD, "params": {},
        "admin_bearer": format!("{id}.{}", token_secret()),
    })
    .to_string();
    // Retry transport/empty-read failures only, never a parseable refusal.
    // Each attempt has its own nonce, so a retry cannot be a replay.
    for _ in 0..3 {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs()
            .to_string();
        let nonce = hex::encode(rand::random::<[u8; 16]>());
        let canonical = format!(
            "two-internal-action/scope-claim/v1\nPOST\n{PATH}\n{timestamp}\n{nonce}\n{}\nread\n{GUILD}",
            body_hash(body.as_bytes())
        );
        let mut mac =
            Hmac::<sha2::Sha256>::new_from_slice(secret().as_bytes()).expect("fixture MAC");
        mac.update(canonical.as_bytes());
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let headers = [
            ("x-two-key-id", SERVICE_ID.to_owned()),
            ("x-two-timestamp", timestamp),
            ("x-two-nonce", nonce.clone()),
            ("x-two-signature", signature),
            ("x-two-scope", "read".to_owned()),
            ("x-two-guild", GUILD.to_owned()),
        ];
        if let Ok(response) = post(addr, &body, &headers) {
            if status(&response).is_some() {
                return (response, nonce);
            }
        }
    }
    panic!("receiver returned no parseable response after three transport attempts");
}

async fn seed(
    pool: &sqlx::PgPool,
    id: &str,
    lifetime: &str,
    expired: bool,
    revoked: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH anchor AS (SELECT now() - interval '5184000 seconds' AS issued)
         INSERT INTO admin_api_tokens
           (token_id, secret_hash, scopes, guild_ids, holder_id, holder_kind,
            issued_at, expires_at, revoked_at)
         SELECT $1, $2, ARRAY['read'], ARRAY[123456789012345678]::bigint[],
                'elapsed-fixture-human', 'human', issued,
                CASE WHEN $4 THEN now() - interval '1 second'
                     ELSE issued + $3::text::interval END,
                CASE WHEN $5 THEN now() - interval '1 second' ELSE NULL END
         FROM anchor",
    )
    .bind(id)
    .bind(body_hash(token_secret().as_bytes()))
    .bind(lifetime)
    .bind(expired)
    .bind(revoked)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
async fn elapsed_lifetime_across_spring_dst_reaches_closed_receiver_not_unauthorized() {
    let db = fixture().await;
    let zone = install_spring_timezone(&db).await;
    let peer = db
        .independent_pool()
        .await
        .expect("fresh fixture connections");
    let observed_zone: String = sqlx::query_scalar("SHOW timezone")
        .fetch_one(&peer)
        .await
        .expect("observe new-connection timezone");
    assert_eq!(
        observed_zone, zone,
        "fresh connections inherit owned database default"
    );

    let cases = [
        (
            "adm_0000000000000001",
            "inside-bound live control",
            "5187600 seconds",
            false,
            false,
            403,
        ),
        (
            "adm_0000000000000002",
            "exact 90 elapsed days",
            "7776000 seconds",
            false,
            false,
            403,
        ),
        (
            "adm_0000000000000003",
            "one microsecond inside bound",
            "7775999.999999 seconds",
            false,
            false,
            403,
        ),
        (
            "adm_0000000000000004",
            "expired control",
            "7776000 seconds",
            true,
            false,
            401,
        ),
        (
            "adm_0000000000000005",
            "revoked control",
            "7776000 seconds",
            false,
            true,
            401,
        ),
    ];
    for (id, _, lifetime, expired, revoked, _) in cases {
        seed(&peer, id, lifetime, expired, revoked)
            .await
            .expect("seed named human authority");
    }
    // This is fixture validation, not the receiver oracle: ensure the accepted
    // exact-bound row is live, finite and really straddles spring DST. The
    // receiver below independently must authorize its actual registry lookup.
    let valid_boundary: bool = sqlx::query_scalar(
        "SELECT holder_kind = 'human' AND holder_id = 'elapsed-fixture-human'
            AND isfinite(issued_at) AND isfinite(expires_at)
            AND issued_at <= now() AND expires_at > now() AND revoked_at IS NULL
            AND extract(epoch FROM expires_at) - extract(epoch FROM issued_at) = 7776000
            AND expires_at > issued_at + interval '90 days'
         FROM admin_api_tokens WHERE token_id = 'adm_0000000000000002'",
    )
    .fetch_one(&peer)
    .await
    .expect("validate exact elapsed boundary and DST witness");
    assert!(
        valid_boundary,
        "fixture must distinguish calendar days from elapsed seconds"
    );

    // Constraints remain enabled: over-bound and nonfinite rows must not enter
    // the registry. Execute errors outside transactions so controls are independent.
    let over_bound = seed(
        &peer,
        "adm_0000000000000006",
        "7776000.000001 seconds",
        false,
        false,
    )
    .await
    .expect_err("registry refuses one microsecond over bound");
    assert_eq!(
        over_bound
            .as_database_error()
            .and_then(|e| e.code())
            .as_deref(),
        Some("23514")
    );
    for (issued, expires) in [
        ("-infinity", "infinity"),
        ("2020-01-01T00:00:00Z", "infinity"),
        ("-infinity", "2020-01-02T00:00:00Z"),
    ] {
        let error = sqlx::query(
            "INSERT INTO admin_api_tokens
             (token_id, secret_hash, scopes, guild_ids, holder_id, holder_kind, issued_at, expires_at)
             VALUES ('adm_0000000000000006', $1, ARRAY['read'], ARRAY[123456789012345678]::bigint[],
                     'elapsed-fixture-human', 'human', $2::text::timestamptz, $3::text::timestamptz)",
        ).bind(body_hash(token_secret().as_bytes())).bind(issued).bind(expires)
            .execute(&peer).await.expect_err("registry refuses nonfinite authority");
        assert_eq!(
            error.as_database_error().and_then(|e| e.code()).as_deref(),
            Some("23514")
        );
    }

    let (bot, addr) = spawn_bot(&db).await;
    let mut observations = Vec::new();
    for (id, label, _, _, _, expected_status) in cases {
        for action in ["event.read", "settings.get"] {
            let (response, nonce) = signed_post(addr, id, action);
            let burns: i64 =
                sqlx::query_scalar("SELECT count(*) FROM internal_nonces WHERE nonce_hash = $1")
                    .bind(body_hash(nonce.as_bytes()))
                    .fetch_one(&peer)
                    .await
                    .expect("nonce witness");
            observations.push((label, action, expected_status, response, burns));
        }
    }
    let intents: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_idempotency")
        .fetch_one(&peer)
        .await
        .expect("intent census");
    let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_action_log")
        .fetch_one(&peer)
        .await
        .expect("execution receipt census");
    // Explicit teardown happens even for the expected RED status mismatch.
    bot.stop().await;
    peer.close().await;
    db.close()
        .await
        .expect("drop only owned disposable database");

    assert_eq!(
        (intents, receipts),
        (0, 0),
        "closed surface produces no intent or execution receipt"
    );
    for (label, action, expected_status, response, burns) in observations {
        assert_eq!(
            burns, 1,
            "{label}/{action}: scoped service MAC passed before registry authorization"
        );
        let payload: Value =
            serde_json::from_str(response.split_once("\r\n\r\n").expect("HTTP body").1)
                .expect("JSON refusal");
        assert!(
            payload.get("result").is_none(),
            "{label}/{action}: no read result"
        );
        assert!(
            !response.contains(&token_secret()),
            "bearer suffix never escapes"
        );
        assert_eq!(
            status(&response),
            Some(expected_status),
            "{label}/{action}: finite elapsed lifetime is timezone-independent; valid authority stays closed403, not401"
        );
        assert_eq!(
            payload["error"]["code"],
            if expected_status == 403 {
                "action_not_allowed"
            } else {
                "unauthorized"
            }
        );
    }
}
