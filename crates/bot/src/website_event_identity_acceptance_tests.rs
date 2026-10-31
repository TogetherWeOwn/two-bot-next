//! Acceptance: the events mirror validates guild and snowflake identities
//! at the REST adapter boundary before anything is normalized or stored.
//!
//! Every wire row must prove it belongs to the configured guild. Event IDs
//! and non-null channel IDs must be canonical decimal, nonzero snowflakes:
//! digit strings with no sign, no whitespace, no leading zeros and no
//! numeric aliases, so `"03000"` can never pass as a second `"3000"`.
//! Any identity failure rejects the whole snapshot with a failed tick and
//! leaves the previous mirror byte-identical. Valid same-guild snapshots,
//! null-channel external events and valid empty arrays keep their current
//! semantics. The pure `normalize_events` fixture contract is out of scope:
//! these tests drive `run_once` so only the adapter boundary is pinned.

use super::*;
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use two_bot_core::apply_web_contract;
use two_bot_testsupport::{guard_database_url, TestDatabase};

use crate::discord_test_common::{MockRest, ScriptedResponse};

const GUILD: &str = "2222";
const FOREIGN_GUILD: &str = "3333";

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy(
        "synthetic-event-identity-test-token".to_owned(),
        Some(mock.origin()),
    )
    .unwrap()
}

/// One realistic Discord scheduled-event wire row. `guild_id: None` omits
/// the key entirely; ids are `Value` so wrong JSON types can be exercised.
fn wire_event(id: Value, guild_id: Option<Value>, channel_id: Value) -> Value {
    let mut row = json!({
        "id": id,
        "name": "Test night",
        "scheduled_start_time": "2026-10-01T19:00:00Z",
        "status": 1,
        "channel_id": channel_id,
        "description": "fixture",
    });
    if let Some(guild_id) = guild_id {
        row["guild_id"] = guild_id;
    }
    row
}

fn valid_event(id: &str, channel_id: Value) -> Value {
    wire_event(json!(id), Some(json!(GUILD)), channel_id)
}

fn foreign_event(id: &str) -> Value {
    wire_event(json!(id), Some(json!(FOREIGN_GUILD)), json!(null))
}

/// Snapshots the identity boundary must reject as a whole. A few entries
/// (non-numeric string id, numeric channel id) are already refused today;
/// they stay as regression anchors beside the new identity cases.
fn identity_invalid_snapshots() -> Vec<(&'static str, Value)> {
    let overflow_id = "9".repeat(26);
    vec![
        ("foreign guild", json!([foreign_event("3005")])),
        (
            "missing guild_id key",
            json!([wire_event(json!("3005"), None, json!(null))]),
        ),
        (
            "null guild_id",
            json!([wire_event(json!("3005"), Some(json!(null)), json!(null))]),
        ),
        (
            "numeric guild_id",
            json!([wire_event(json!("3005"), Some(json!(2222)), json!(null))]),
        ),
        (
            "zero guild_id",
            json!([wire_event(json!("3005"), Some(json!("0")), json!(null))]),
        ),
        (
            "leading-zero guild alias",
            json!([wire_event(json!("3005"), Some(json!("02222")), json!(null))]),
        ),
        (
            "non-numeric guild_id",
            json!([wire_event(json!("3005"), Some(json!("bad")), json!(null))]),
        ),
        (
            "empty guild_id",
            json!([wire_event(json!("3005"), Some(json!("")), json!(null))]),
        ),
        (
            "guild_id with trailing space",
            json!([wire_event(json!("3005"), Some(json!("2222 ")), json!(null))]),
        ),
        (
            "valid row beside foreign row",
            json!([valid_event("3006", json!(null)), foreign_event("3007")]),
        ),
        (
            "foreign row beside valid row",
            json!([foreign_event("3007"), valid_event("3006", json!(null))]),
        ),
        (
            "valid row beside missing-guild row",
            json!([
                valid_event("3006", json!(null)),
                wire_event(json!("3007"), None, json!(null)),
            ]),
        ),
        ("zero event id", json!([valid_event("0", json!(null))])),
        (
            "leading-zero event id",
            json!([valid_event("03008", json!(null))]),
        ),
        (
            "signed event id",
            json!([valid_event("+3008", json!(null))]),
        ),
        (
            "negative event id",
            json!([valid_event("-3008", json!(null))]),
        ),
        (
            "event id with leading space",
            json!([valid_event(" 3008", json!(null))]),
        ),
        ("empty event id", json!([valid_event("", json!(null))])),
        (
            "non-numeric event id",
            json!([valid_event("bad", json!(null))]),
        ),
        (
            "numeric event id",
            json!([wire_event(json!(3008), Some(json!(GUILD)), json!(null))]),
        ),
        (
            "event id beyond u64",
            json!([valid_event(&overflow_id, json!(null))]),
        ),
        (
            "numeric-alias duplicate event ids",
            json!([
                valid_event("3009", json!(null)),
                valid_event("03009", json!(null)),
            ]),
        ),
        ("zero channel id", json!([valid_event("3010", json!("0"))])),
        (
            "leading-zero channel id",
            json!([valid_event("3010", json!("04000"))]),
        ),
        (
            "non-numeric channel id",
            json!([valid_event("3010", json!("bad"))]),
        ),
        ("empty channel id", json!([valid_event("3010", json!(""))])),
        (
            "numeric channel id",
            json!([valid_event("3010", json!(4000))]),
        ),
        (
            "valid row beside invalid-channel row",
            json!([
                valid_event("3011", json!("4001")),
                valid_event("3012", json!("bad")),
            ]),
        ),
    ]
}

async fn tick_events(
    pool: &PgPool,
    rest: &ActionExecutor,
    observation: &Mutex<()>,
    shutdown: &watch::Receiver<bool>,
) -> Result<(), ErrorClass> {
    run_once(
        Kind::Events,
        pool,
        rest,
        GUILD,
        observation,
        shutdown,
        false,
    )
    .await
}

/// A lazy pool never connects unless rejected work reaches storage. Its
/// short acquire timeout bounds a regression, and only the test service
/// is named. Mirrors the poison pool in `website_snapshot_tests`.
fn poison_pool() -> PgPool {
    guard_database_url("postgres://agent_test:@agent-testdb:5432/two_bot_test_poison")
        .expect("poison pool must reject ambient PostgreSQL connection settings");
    PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(20))
        .connect_lazy_with(
            PgConnectOptions::new_without_pgpass()
                .host("agent-testdb")
                .port(5432)
                .username("agent_test")
                .password("")
                .database("two_bot_test_poison"),
        )
}

/// Identity validation happens before storage: every invalid snapshot
/// fails as a REST error while the pool can never connect.
#[tokio::test]
async fn identity_invalid_snapshots_never_reach_storage() {
    let snapshots = identity_invalid_snapshots();
    let mock = MockRest::start(
        snapshots
            .iter()
            .map(|(_, snapshot)| ScriptedResponse::json(200, snapshot.clone()))
            .collect(),
        ScriptedResponse::status(403),
    )
    .await;
    let pool = poison_pool();
    let observation = Mutex::new(());
    let (_stop, shutdown) = watch::channel(false);
    for (label, _) in &snapshots {
        assert_eq!(
            tick_events(&pool, &executor(&mock), &observation, &shutdown).await,
            Err(ErrorClass::Rest),
            "{label} must fail the tick before storage"
        );
    }
    assert_eq!(mock.requests().len(), snapshots.len());
    mock.shutdown().await;
    pool.close().await;
}

/// The full mirror row set for the configured guild, including the
/// observation timestamp, so a rejected snapshot that republishes or
/// partially replaces rows is visible.
async fn mirror_rows(pool: &PgPool) -> Vec<(String, Option<String>, String)> {
    sqlx::query_as(
        "SELECT event_id, channel_id, updated_at FROM scheduled_events \
         WHERE guild_id=$1 ORDER BY event_id",
    )
    .bind(GUILD)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Seeded mirror survives every identity-invalid snapshot unchanged, then
/// valid same-guild snapshots keep their semantics: atomic replacement,
/// null-channel external events, and empty-array clearing.
#[tokio::test]
async fn identity_invalid_snapshots_preserve_mirror_and_valid_controls_publish() {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP event identity DB integration: TWO_TEST_DATABASE_URL is not set");
        return;
    };
    let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated agent-testdb fixture");
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();

    let invalid = identity_invalid_snapshots();
    let mut responses = vec![ScriptedResponse::json(
        200,
        json!([valid_event("3000", json!("4000"))]),
    )];
    responses.extend(
        invalid
            .iter()
            .map(|(_, snapshot)| ScriptedResponse::json(200, snapshot.clone())),
    );
    responses.extend([
        // Valid controls, in order: null-channel replacement, a mixed
        // valid pair (one voice channel, one external), empty clearing.
        ScriptedResponse::json(200, json!([valid_event("3001", json!(null))])),
        ScriptedResponse::json(
            200,
            json!([
                valid_event("3002", json!("4001")),
                valid_event("3003", json!(null)),
            ]),
        ),
        ScriptedResponse::json(200, json!([])),
    ]);
    let expected_requests = responses.len();
    let mock = MockRest::start(responses, ScriptedResponse::status(500)).await;
    let rest = executor(&mock);
    let observation = Mutex::new(());
    let (_stop, shutdown) = watch::channel(false);

    tick_events(&pool, &rest, &observation, &shutdown)
        .await
        .expect("valid same-guild seed snapshot publishes");
    let seeded = mirror_rows(&pool).await;
    assert_eq!(seeded.len(), 1, "seed publishes exactly one row");
    assert_eq!(seeded[0].0, "3000");
    assert_eq!(seeded[0].1.as_deref(), Some("4000"));

    for (label, _) in &invalid {
        assert_eq!(
            tick_events(&pool, &rest, &observation, &shutdown).await,
            Err(ErrorClass::Rest),
            "{label} must produce a failed tick"
        );
        assert_eq!(
            mirror_rows(&pool).await,
            seeded,
            "{label} must leave the entire previous mirror unchanged"
        );
    }

    tick_events(&pool, &rest, &observation, &shutdown)
        .await
        .expect("valid null-channel snapshot publishes");
    let rows = mirror_rows(&pool).await;
    assert_eq!(rows.len(), 1, "replacement is atomic");
    assert_eq!(rows[0].0, "3001");
    assert_eq!(rows[0].1, None, "external event keeps a null channel");

    tick_events(&pool, &rest, &observation, &shutdown)
        .await
        .expect("valid mixed-channel snapshot publishes");
    let rows = mirror_rows(&pool).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "3002");
    assert_eq!(rows[0].1.as_deref(), Some("4001"));
    assert_eq!(rows[1].0, "3003");
    assert_eq!(rows[1].1, None);

    tick_events(&pool, &rest, &observation, &shutdown)
        .await
        .expect("valid empty snapshot publishes");
    assert!(
        mirror_rows(&pool).await.is_empty(),
        "valid empty response clears the mirror"
    );

    assert_eq!(mock.requests().len(), expected_requests);
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}
