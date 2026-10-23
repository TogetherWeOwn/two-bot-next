//! Failing-first acceptance for Slice 0a: the Rust runtime merges the DO
//! `/ops/config` record as the top settings layer (TOG-36209; builder
//! TOG-36216 must not edit this file).
//!
//! Contract under test (builder: implement exactly this observable behavior;
//! internals — module layout, HTTP client, cache struct — are your choice):
//!
//! - **Source.** At boot and on **every** live-poll tick, the runtime GETs the
//!   URL in `TWO_OPS_CONFIG_URL` as-is (no path suffix, no query added) and
//!   parses the Worker's `GET /ops/config` shape
//!   `{epoch, overrides, ...}`. Extra fields (`deploymentId`, `baseline`,
//!   `enforcement`) are ignored. Every mock response below carries them, so a
//!   strict-shape parser fails these pins.
//! - **Merge order.** Per key, highest present layer wins: compiled default <
//!   process env (boot) < guild row < /ops override. The override is visible
//!   through `get`, `snapshot().get` and `env_snapshot` alike — a reader that
//!   bypasses any one of them is an incomplete enforcement. Ops values are
//!   strings end to end: the reader serves `Value::String`, never a coerced
//!   number or a split list.
//! - **No restart.** Clearing a key (a later epoch whose `overrides` omit it)
//!   falls back to the guild row on the next tick, same process, same poll.
//! - **Epoch safety.** A record applies iff its epoch is newer than the last
//!   applied one; an older epoch is ignored and the last good record stays
//!   live. An epoch-0 empty record is transparent (as if no overrides).
//! - **Fetch failure is not fatal.** A 5xx, a malformed body, or an
//!   unreachable endpoint keeps the last good record and the tick still
//!   returns `Ok`: an ops outage must never wedge the guild-settings poll.
//! - **Reach keys never surface.** Overrides naming EnvOnly/unknown keys are
//!   dropped, even from a record written before the PUT allowlist narrowed:
//!   `TWO_MODERATION`, `TWO_MODERATION_PROTECTED_ROLE_IDS`, `TWO_OWEN_USER_ID`,
//!   `TWO_ONBOARDING_MODE`, `TWO_ANTI_NUKE_PROTECTED_USER_IDS`,
//!   `TWO_ANTI_NUKE_TRUSTED_USER_IDS`, `TWO_TEMP_VOICE_SYNTHETIC_HUMAN_IDS`,
//!   `TWO_ASSISTANT_ENDPOINT`, `TWO_ASSISTANT_MODEL`, `TWO_INTERNAL_*`, and
//!   uncatalogued names. Mirrors the Worker allowlist pins in
//!   `wrangler/test/ops-override-allowlist-acceptance.test.ts`.
//! - **Awaited read.** The tick awaits the ops read: reader state after
//!   `tick().await` is final. A background-refresh design races these pins.
//! - **Unmoved marks still poll ops.** The guild early-return on unmoved
//!   revision/row-count must not skip the ops read; an ops-only change takes
//!   effect on the next tick.
//!
//! Deliberately NOT pinned here (builder covers in own unit tests + review):
//! absent/empty/malformed `TWO_OPS_CONFIG_URL` (disabled layer), the
//! `Authorization: Bearer` send on the production fetch (the loopback mock
//! accepts unauthenticated reads so this test never dictates the token's env
//! name), refused/timeout fetch shapes, the equal-epoch re-read rule, the
//! max-staleness expiry boundary with injected time (plus its documenting
//! rustdoc — the "documented bound" half of the criterion), and the stale
//! log line. New `TWO_*` names the implementation reads must be classified
//! in `wrangler/src/container-env.ts` (`TWO_OPS_CONFIG_URL` included).
//!
//! Database cases run only against the guarded disposable fixture
//! (`TWO_TEST_DATABASE_URL`, agent-testdb / CI Postgres): skipped locally
//! when unset, a hard failure in CI without it. No Discord endpoint and no
//! Worker is contacted; the ops endpoint is a loopback axum mock.
//!
//! Wiring: this module is a child of the `settings_jobs` test module (see the
//! `#[path]` declaration in `settings_jobs_tests.rs`) so it can drive the
//! private poll against a disposable pool without touching the process-global
//! live reader. Do not load it as a second module elsewhere.

use std::collections::HashMap;
use std::sync::Arc;

use axum::response::IntoResponse;
use serde_json::json;
use tokio::sync::RwLock;
use two_bot_cutover::settings::SettingsStore;
use two_bot_testsupport::TestDatabase;

use super::super::Poll;

/// Full GET URL of the ops record. New runtime input (TOG-36209): the
/// container-env drift test stays red until the builder classifies it.
const OPS_URL_ENV: &str = "TWO_OPS_CONFIG_URL";

const GUILD: &str = "g1";
const KEY: &str = "TWO_AUTOMOD_MENTION_LIMIT";
const KEY2: &str = "TWO_RAID_JOIN_THRESHOLD";
const LIST_KEY: &str = "TWO_AUTOMOD_BYPASS_ROLE_IDS";

/// Reach/identity keys that must never surface from an ops record, even one
/// written before the PUT allowlist narrowed. Every name is already
/// classified in `wrangler/src/container-env.ts`, so these literals alone
/// keep the drift test quiet; only `OPS_URL_ENV` above is new.
const REACH_KEYS: &[&str] = &[
    "TWO_MODERATION",
    "TWO_MODERATION_PROTECTED_ROLE_IDS",
    "TWO_OWEN_USER_ID",
    "TWO_ONBOARDING_MODE",
    "TWO_ANTI_NUKE_PROTECTED_USER_IDS",
    "TWO_ANTI_NUKE_TRUSTED_USER_IDS",
    "TWO_TEMP_VOICE_SYNTHETIC_HUMAN_IDS",
    "TWO_ASSISTANT_ENDPOINT",
    "TWO_ASSISTANT_MODEL",
    "TWO_INTERNAL_ALLOW_SETTINGS",
];

/// Uncatalogued names fail closed like declared EnvOnly ones.
const UNKNOWN_KEY: &str = "TWO_NEW_GATE";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MockMode {
    Serve,
    Malformed,
    Fail,
}

#[derive(Debug, Clone)]
struct MockOps {
    epoch: u64,
    overrides: HashMap<String, String>,
    mode: MockMode,
}

async fn set_record(state: &Arc<RwLock<MockOps>>, epoch: u64, overrides: &[(&str, &str)]) {
    let mut mock = state.write().await;
    mock.mode = MockMode::Serve;
    mock.epoch = epoch;
    mock.overrides = overrides
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
}

async fn set_mode(state: &Arc<RwLock<MockOps>>, mode: MockMode) {
    state.write().await.mode = mode;
}

async fn ops_config_handler(
    axum::extract::State(state): axum::extract::State<Arc<RwLock<MockOps>>>,
) -> axum::response::Response {
    let mock = state.read().await.clone();
    match mock.mode {
        MockMode::Serve => axum::Json(json!({
            "deploymentId": "mock-ops-deployment",
            "epoch": mock.epoch,
            "overrides": mock.overrides,
            "baseline": { "TWO_AUTOMOD": "1" },
            "enforcement": "pending",
        }))
        .into_response(),
        MockMode::Malformed => "not-json{{{".into_response(),
        MockMode::Fail => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[tokio::test]
async fn ops_overrides_merge_above_guild_rows_with_epoch_safety() {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP ops override acceptance: TWO_TEST_DATABASE_URL is not set");
        return;
    };
    let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated agent-testdb fixture");
    let pool = fixture.pool().clone();

    // Guild baseline underneath every override phase.
    let store = SettingsStore::new(&pool);
    store
        .set(GUILD, KEY, Some(json!(5)), "test")
        .await
        .expect("guild baseline write");
    store
        .set(GUILD, KEY2, Some(json!(10)), "test")
        .await
        .expect("guild second-key write");

    // Loopback stand-in for the Worker's GET /ops/config. Serves the full
    // Worker shape (extras included) from mutable state; the production
    // fetch sends a bearer the mock never requires.
    let mock = Arc::new(RwLock::new(MockOps {
        epoch: 0,
        overrides: HashMap::new(),
        mode: MockMode::Serve,
    }));
    let app = axum::Router::new()
        .route("/ops/config", axum::routing::get(ops_config_handler))
        .with_state(Arc::clone(&mock));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock ops listen");
    let ops_url = format!(
        "http://{}/ops/config",
        listener.local_addr().expect("mock ops address")
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock ops serve");
    });
    std::env::set_var(OPS_URL_ENV, &ops_url);

    // Private poll + private reader: no process-global live reader involved.
    let (poll, reader) = Poll::with_pool(pool.clone());

    // Epoch 0 with no overrides is transparent: the guild baseline shows.
    poll.tick().await.expect("baseline tick");
    assert_eq!(reader.get(GUILD, KEY), Some(json!(5)));
    assert_eq!(reader.get(GUILD, KEY2), Some(json!(10)));

    // Override wins while set, on every read API; other guild keys stay.
    set_record(&mock, 1, &[(KEY, "9")]).await;
    poll.tick().await.expect("override tick");
    assert_eq!(reader.get(GUILD, KEY), Some(json!("9")));
    assert_eq!(reader.snapshot().get(GUILD, KEY), Some(&json!("9")));
    assert_eq!(
        reader
            .env_snapshot(Some(GUILD))
            .get(KEY)
            .map(String::as_str),
        Some("9")
    );
    assert_eq!(reader.get(GUILD, KEY2), Some(json!(10)));

    // List values stay verbatim strings, never JSON-mangled.
    set_record(&mock, 2, &[(KEY, "9"), (LIST_KEY, "11,22")]).await;
    poll.tick().await.expect("list override tick");
    assert_eq!(reader.get(GUILD, LIST_KEY), Some(json!("11,22")));
    assert_eq!(
        reader
            .env_snapshot(Some(GUILD))
            .get(LIST_KEY)
            .map(String::as_str),
        Some("11,22")
    );

    // Clearing (a later epoch omitting the key) falls back to the guild row
    // with no restart; keys with no guild row go fully absent.
    set_record(&mock, 3, &[]).await;
    poll.tick().await.expect("clear tick");
    assert_eq!(reader.get(GUILD, KEY), Some(json!(5)));
    assert_eq!(
        reader
            .env_snapshot(Some(GUILD))
            .get(KEY)
            .map(String::as_str),
        Some("5")
    );
    assert_eq!(reader.get(GUILD, LIST_KEY), None);
    assert!(!reader.env_snapshot(Some(GUILD)).contains_key(LIST_KEY));

    // A stale epoch keeps the last good record on every read API.
    set_record(&mock, 5, &[(KEY, "7")]).await;
    poll.tick().await.expect("fresh epoch tick");
    assert_eq!(reader.get(GUILD, KEY), Some(json!("7")));
    set_record(&mock, 3, &[(KEY, "1")]).await;
    poll.tick().await.expect("stale epoch tick");
    assert_eq!(reader.get(GUILD, KEY), Some(json!("7")));
    assert_eq!(reader.snapshot().get(GUILD, KEY), Some(&json!("7")));
    assert_eq!(
        reader
            .env_snapshot(Some(GUILD))
            .get(KEY)
            .map(String::as_str),
        Some("7")
    );

    // Fetch failures keep the last good record and never fail the tick.
    set_mode(&mock, MockMode::Fail).await;
    poll.tick().await.expect("tick stays Ok on ops outage");
    poll.tick()
        .await
        .expect("repeated tick stays Ok on ops outage");
    assert_eq!(reader.get(GUILD, KEY), Some(json!("7")));
    set_mode(&mock, MockMode::Malformed).await;
    poll.tick()
        .await
        .expect("tick stays Ok on malformed ops body");
    assert_eq!(reader.get(GUILD, KEY), Some(json!("7")));

    // Reach/identity/unknown override keys never surface, while a
    // behavioral key in the same record still applies.
    let mut reach_record = vec![(KEY, "8")];
    reach_record.extend(REACH_KEYS.iter().map(|key| (*key, "reach-fixture")));
    reach_record.push((UNKNOWN_KEY, "unknown-fixture"));
    set_record(&mock, 6, &reach_record).await;
    poll.tick().await.expect("reach record tick");
    assert_eq!(reader.get(GUILD, KEY), Some(json!("8")));
    for key in REACH_KEYS
        .iter()
        .copied()
        .chain(std::iter::once(UNKNOWN_KEY))
    {
        assert_eq!(reader.get(GUILD, key), None, "{key} must never surface");
        assert_eq!(
            reader.snapshot().get(GUILD, key),
            None,
            "{key} must never surface"
        );
        assert!(
            !reader.env_snapshot(Some(GUILD)).contains_key(key),
            "{key} must never surface"
        );
    }

    server.abort();
    std::env::remove_var(OPS_URL_ENV);
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}
