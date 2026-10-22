//! Acceptance tests: each EventSub boot failure refuses startup with its own
//! `receiver_*` error class.
//!
//! Five distinct startup causes share one class today
//! (`receiver_prerequisites_invalid`), so the fatal log line cannot say which
//! prerequisite failed. These tests pen the five stable classes the boot must
//! refuse with instead — one per cause — plus the catalog rows and the fatal
//! exit-1 behavior. They are red until the Builder lands the split on this
//! branch; the Builder must satisfy them **without editing this file**:
//!
//! - missing store (delivery enabled, no database pool): `receiver_store_missing`
//! - missing/empty `DISCORD_TOKEN`: `receiver_token_missing`
//! - boot identity refused the Announcements capability: `receiver_identity_refused`
//! - token rejected by send-admission key derivation: `receiver_admission_rejected`
//! - malformed `DISCORD_API_BASE` proxy override: `receiver_proxy_invalid`
//!
//! Contract notes for the Builder:
//!
//! - `build_delivery_deps` keeps its `(pool, token, proxy, config,
//!   announcements_permitted) -> Result<_, &'static str>` shape; only the `Err`
//!   literals change. The identity check stays first (before key derivation).
//! - The missing-token check must come before the missing-store check: with no
//!   token the gateway prerequisites already fail, so the store is `None` too,
//!   and the token cause would otherwise stay hidden behind the store refusal.
//!   The two stay distinguishable from `Config` alone (token set/database
//!   missing vs database set/token missing); both-missing is unpinned.
//! - The catalog gains one `| class | meaning | next action |` row per class in
//!   `docs/observability-event-catalog.md` (backticked class, non-empty meaning
//!   and next-action cells). Suggested meanings: store = delivery enabled but
//!   no database pool, provision `DATABASE_URL`/`GUILD_ID`; token = delivery
//!   enabled but `DISCORD_TOKEN` missing or empty, set the secret; identity =
//!   boot identity lacks Announcements for go-live delivery, check activation;
//!   admission = token failed send-admission key derivation, check token
//!   format/whitespace; proxy = `DISCORD_API_BASE` unusable, fix proxy config.
//! - The internal-action `receiver_prerequisites_invalid` sites (a different
//!   "internal-action startup refused" message) are out of scope: only the
//!   EventSub delivery sites must stop emitting the legacy class.
//! - The identity/admission/proxy literals live in `eventsub_delivery.rs`, not
//!   `main.rs`, so a `main.rs`-only presence check cannot cover them; both
//!   files are pinned here.
//!
//! Offline: lazy pools never connect, spawned children exit before any
//! database or Discord contact, and every secret is a synthetic fixture.

use std::collections::HashMap;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Delivery enabled but no database pool (gateway prerequisites unmet).
const RECEIVER_STORE_MISSING: &str = "receiver_store_missing";
/// Delivery enabled but `DISCORD_TOKEN` missing or empty.
const RECEIVER_TOKEN_MISSING: &str = "receiver_token_missing";
/// Boot identity refused the Announcements capability for go-live delivery.
const RECEIVER_IDENTITY_REFUSED: &str = "receiver_identity_refused";
/// Token rejected by send-admission key derivation (token format).
const RECEIVER_ADMISSION_REJECTED: &str = "receiver_admission_rejected";
/// `DISCORD_API_BASE` override unusable (proxy config).
const RECEIVER_PROXY_INVALID: &str = "receiver_proxy_invalid";
/// The single class all five causes share before the split.
const LEGACY_SHARED_CLASS: &str = "receiver_prerequisites_invalid";

const PENNED_CLASSES: [&str; 5] = [
    RECEIVER_STORE_MISSING,
    RECEIVER_TOKEN_MISSING,
    RECEIVER_IDENTITY_REFUSED,
    RECEIVER_ADMISSION_REJECTED,
    RECEIVER_PROXY_INVALID,
];

// `crates/bot` is a binary-only crate, so an integration test cannot `use` its
// items. Including the real `eventsub_delivery.rs` compiles the acceptance
// target from current source (Builder changes track automatically), but
// `cfg(test)` also compiles that file's unit-test module, which references
// `crate::gateway` and `crate::activation`. The stub modules below provide
// exactly those references; the dragged unit tests duplicate the binary's own
// suite and are not the acceptance verdict — the `receiver_*` tests are.
// The included module's `DeliveryDeps::{ledger, sender}` fields are only
// *constructed* here (behavior is asserted through `Err` classes and
// `config`), so allow the resulting dead-code: without this, clippy
// `-D warnings` fails this harness-only target.
#[allow(dead_code)]
#[path = "../src/eventsub_delivery.rs"]
mod eventsub_delivery;

mod gateway {
    /// Mirror of `crates/bot/src/gateway.rs`: install the sole rustls provider
    /// so key derivation in the included module works.
    pub fn ensure_crypto_provider() {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
    }
}

mod activation {
    /// Capability decisions for the dragged unit tests only: staging permits,
    /// every refused identity denies. The real activation logic is pinned by
    /// the binary's own suite, not here.
    pub mod fixtures {
        pub struct StubActivation(bool);

        impl StubActivation {
            pub fn go_live_delivery_permitted(&self) -> bool {
                self.0
            }
        }

        pub fn staging() -> StubActivation {
            StubActivation(true)
        }

        pub fn refused() -> Vec<(&'static str, StubActivation)> {
            vec![("stub-refused-identity", StubActivation(false))]
        }
    }
}

fn delivery_vars() -> HashMap<String, String> {
    HashMap::from([
        ("TWO_EVENTSUB_DELIVERY".to_owned(), "1".to_owned()),
        ("TWO_TWITCH_BROADCASTERS".to_owned(), "222,111".to_owned()),
        (
            "TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID".to_owned(),
            "777".to_owned(),
        ),
        (
            "TWO_EVENTSUB_INVITE_URL".to_owned(),
            "https://discord.gg/example".to_owned(),
        ),
    ])
}

fn delivery_config() -> two_bot_core::eventsub_delivery_config::EventsubDeliveryConfig {
    two_bot_core::eventsub_delivery_config::EventsubDeliveryConfig::from_map(&delivery_vars())
        .expect("valid snapshot")
        .expect("enabled")
}

fn lazy_pool() -> sqlx::PgPool {
    // Never connects: pool construction performs no I/O. Still requires a
    // Tokio context (sqlx `connect_lazy`), so every caller is `#[tokio::test]`.
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://agent_test@agent-testdb/agent_test")
        .expect("lazy pool never connects")
}

#[tokio::test]
async fn refused_identity_refuses_with_the_identity_class_before_key_derivation() {
    gateway::ensure_crypto_provider();
    // A refused identity fails before any key derivation, so even a token the
    // admission would reject still reports the identity cause.
    for token in ["synthetic-delivery-token", "has space"] {
        match eventsub_delivery::build_delivery_deps(
            lazy_pool(),
            token,
            None,
            &delivery_config(),
            false,
        ) {
            Err(class) => assert_eq!(
                class, RECEIVER_IDENTITY_REFUSED,
                "token {token:?} under a refused identity"
            ),
            Ok(_) => panic!(
                "token {token:?} under a refused identity must refuse with \
                 {RECEIVER_IDENTITY_REFUSED}, but it built"
            ),
        }
    }
}

#[tokio::test]
async fn admission_rejection_refuses_with_the_admission_class() {
    gateway::ensure_crypto_provider();
    match eventsub_delivery::build_delivery_deps(
        lazy_pool(),
        "has space",
        None,
        &delivery_config(),
        true,
    ) {
        Err(class) => assert_eq!(class, RECEIVER_ADMISSION_REJECTED),
        Ok(_) => panic!(
            "admission-rejected token must refuse with \
             {RECEIVER_ADMISSION_REJECTED}, but it built"
        ),
    }
}

#[tokio::test]
async fn malformed_proxy_refuses_with_the_proxy_class() {
    gateway::ensure_crypto_provider();
    match eventsub_delivery::build_delivery_deps(
        lazy_pool(),
        "synthetic-delivery-token",
        Some("http://exa mple.com".to_owned()),
        &delivery_config(),
        true,
    ) {
        Err(class) => assert_eq!(class, RECEIVER_PROXY_INVALID),
        Ok(_) => panic!(
            "malformed proxy must refuse with {RECEIVER_PROXY_INVALID}, \
             but it built"
        ),
    }
}

#[tokio::test]
async fn valid_delivery_inputs_still_build() {
    // Control: proves the harness (include, lazy pool, delivery config)
    // compiles and runs, so a red run above is behavioral, not infra.
    gateway::ensure_crypto_provider();
    let deps = eventsub_delivery::build_delivery_deps(
        lazy_pool(),
        "synthetic-delivery-token",
        None,
        &delivery_config(),
        true,
    )
    .expect("synthetic inputs build");
    assert_eq!(deps.config.broadcasters, ["111", "222"]);
    assert_eq!(deps.config.invite_url, "https://discord.gg/example");
    assert_eq!(deps.config.channel_id, "777");
}

#[test]
fn penned_receiver_classes_are_distinct_and_stable() {
    // Vocabulary contract: five pairwise-distinct `receiver_*` classes, none
    // of them the legacy shared class.
    for (index, class) in PENNED_CLASSES.iter().enumerate() {
        assert!(
            class.starts_with("receiver_"),
            "penned class must keep the receiver_ prefix: {class}"
        );
        assert_ne!(
            *class, LEGACY_SHARED_CLASS,
            "penned class must differ from the legacy shared class"
        );
        for other in &PENNED_CLASSES[index + 1..] {
            assert_ne!(*class, *other, "penned classes must be distinct");
        }
    }
}

struct SpawnedExit {
    code: Option<i32>,
    logs: String,
}

fn combined_logs(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn reserved_eventsub_bind() -> String {
    // Parse-only: the child refuses before binding, so the port is never
    // bound and a stolen reservation cannot flake the suite.
    let reserved = TcpListener::bind("127.0.0.1:0").expect("reserve test port");
    let bind = reserved.local_addr().expect("test port addr").to_string();
    drop(reserved);
    bind
}

fn boot_with(vars: &[(&str, &str)]) -> SpawnedExit {
    let bind = reserved_eventsub_bind();
    let mut child = Command::new(env!("CARGO_BIN_EXE_two-bot"))
        .env_clear()
        .env("LISTEN_ADDR", "127.0.0.1:0")
        .env("GUILD_ID", "123")
        .env("TWO_EVENTSUB", "1")
        .env("TWO_EVENTSUB_BIND", bind)
        .env("TWO_TWITCH_WEBHOOK_SECRET", "synthetic-webhook-secret-01")
        .env("TWO_EVENTSUB_DELIVERY", "1")
        .env("TWO_TWITCH_BROADCASTERS", "222,111")
        .env("TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID", "777")
        .env("TWO_EVENTSUB_INVITE_URL", "https://discord.gg/example")
        .envs(vars.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start test bot");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().expect("poll test bot").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill hung test bot");
            let output = child.wait_with_output().expect("reap hung test bot");
            panic!(
                "boot refusal never exited; partial logs: {}",
                combined_logs(&output)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().expect("reap test bot");
    SpawnedExit {
        code: output.status.code(),
        logs: combined_logs(&output),
    }
}

fn assert_refused_with(exit: &SpawnedExit, expected: &str) {
    assert_eq!(
        exit.code,
        Some(1),
        "boot refusal must stay a fatal exit 1; logs: {}",
        exit.logs
    );
    assert!(
        exit.logs.contains("eventsub startup refused"),
        "refusal must keep its fatal message; logs: {}",
        exit.logs
    );
    let field = format!("error_class=\"{expected}\"");
    assert!(
        exit.logs.contains(&field),
        "refusal must carry {field}; logs: {}",
        exit.logs
    );
    // Exactly one class on the line: no sibling cause and no legacy class.
    for other in PENNED_CLASSES
        .iter()
        .chain(std::iter::once(&LEGACY_SHARED_CLASS))
    {
        if *other == expected {
            continue;
        }
        let other_field = format!("error_class=\"{other}\"");
        assert!(
            !exit.logs.contains(&other_field),
            "refusal must not carry {other_field}; logs: {}",
            exit.logs
        );
    }
}

#[test]
fn boot_without_a_store_refuses_with_the_store_class() {
    // Token and guild present, `DATABASE_URL` absent: no pool exists and none
    // is contacted (gateway prerequisites fail before any connect).
    let exit = boot_with(&[("DISCORD_TOKEN", "synthetic-boot-token")]);
    assert_refused_with(&exit, RECEIVER_STORE_MISSING);
}

#[test]
fn boot_with_an_empty_database_url_refuses_with_the_store_class() {
    let exit = boot_with(&[
        ("DISCORD_TOKEN", "synthetic-boot-token"),
        ("DATABASE_URL", ""),
    ]);
    assert_refused_with(&exit, RECEIVER_STORE_MISSING);
}

#[test]
fn boot_without_a_token_refuses_with_the_token_class() {
    // Database URL present but never contacted: prerequisites fail on the
    // missing token before any connect, so the fixture URL is inert.
    let exit = boot_with(&[("DATABASE_URL", "synthetic-database-must-not-connect")]);
    assert_refused_with(&exit, RECEIVER_TOKEN_MISSING);
}

#[test]
fn boot_with_an_empty_token_refuses_with_the_token_class() {
    let exit = boot_with(&[
        ("DISCORD_TOKEN", ""),
        ("DATABASE_URL", "synthetic-database-must-not-connect"),
    ]);
    assert_refused_with(&exit, RECEIVER_TOKEN_MISSING);
}

fn crate_file(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("test input unreadable at {} ({err})", path.display()))
}

fn catalog_doc() -> String {
    crate_file("../../docs/observability-event-catalog.md")
}

/// A `| class | meaning | next action |` row: the class plus at least two
/// further non-empty cells, so a bare list mention fails.
fn catalog_has_row_for(catalog: &str, class: &str) -> bool {
    catalog.lines().any(|line| {
        if !line.trim_start().starts_with('|') || !line.contains(class) {
            return false;
        }
        line.split('|')
            .filter(|cell| !cell.trim().is_empty())
            .count()
            >= 3
    })
}

#[test]
fn penned_classes_are_catalogued_with_meaning_and_next_action() {
    let catalog = catalog_doc();
    for class in PENNED_CLASSES {
        let backticked = format!("`{class}`");
        assert!(
            catalog.contains(&backticked),
            "catalog must record the class scalar: {backticked}"
        );
        assert!(
            catalog_has_row_for(&catalog, class),
            "catalog must carry a meaning + next-action row for {class}"
        );
    }
}

fn delivery_fence_region(main: &str) -> String {
    // The `let delivery = ...map` closure up to the bind call, `//` comments
    // stripped so a comment naming a class cannot satisfy the pin.
    let fence = main
        .split_once("let delivery = eventsub_delivery_config.map")
        .expect("delivery fence site moved")
        .1
        .split_once("bind_with_delivery")
        .expect("delivery bind site moved")
        .0;
    fence
        .lines()
        .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn delivery_refuse_sites_emit_no_legacy_class() {
    // Every EventSub delivery site splits: the legacy shared class survives
    // only at the out-of-scope internal-action sites, which this pin does not
    // scan.
    let delivery = crate_file("src/eventsub_delivery.rs");
    assert!(
        !delivery.contains(LEGACY_SHARED_CLASS),
        "eventsub_delivery.rs must emit no {LEGACY_SHARED_CLASS}"
    );
    let fence = delivery_fence_region(&crate_file("src/main.rs"));
    assert!(
        !fence.contains(LEGACY_SHARED_CLASS),
        "the delivery fence in main.rs must emit no {LEGACY_SHARED_CLASS}"
    );
}

#[test]
fn penned_classes_are_emitted_at_the_refuse_sites() {
    // Presence pin so the no-legacy pin above cannot go green by deleting
    // literals: each penned class is emitted where its cause is detected.
    let main = crate_file("src/main.rs");
    for class in [RECEIVER_STORE_MISSING, RECEIVER_TOKEN_MISSING] {
        let literal = format!("\"{class}\"");
        assert!(
            main.contains(&literal),
            "main.rs must emit the {class} literal"
        );
    }
    let delivery = crate_file("src/eventsub_delivery.rs");
    for class in [
        RECEIVER_IDENTITY_REFUSED,
        RECEIVER_ADMISSION_REJECTED,
        RECEIVER_PROXY_INVALID,
    ] {
        let literal = format!("\"{class}\"");
        assert!(
            delivery.contains(&literal),
            "eventsub_delivery.rs must emit the {class} literal"
        );
    }
}

#[test]
fn penned_classes_are_pinned_by_the_conformance_suite() {
    // The fail-closed conformance suite must list the new classes, so a later
    // rename or removal turns it red alongside the catalog.
    let conformance = crate_file("src/observability_event_conformance_tests.rs");
    for class in PENNED_CLASSES {
        let literal = format!("\"{class}\"");
        assert!(
            conformance.contains(&literal),
            "the conformance suite must pin the {class} literal"
        );
    }
}

#[test]
fn delivery_refusal_still_exits_through_the_fatal_refuse() {
    // Control: the split renames classes, it does not soften the fatal path.
    let fence = delivery_fence_region(&crate_file("src/main.rs"));
    assert!(
        fence.contains("eventsub startup refused"),
        "the delivery fence must keep its fatal message"
    );
    assert!(
        fence.contains("process::exit(1)"),
        "the delivery fence must keep its fatal exit"
    );
}
