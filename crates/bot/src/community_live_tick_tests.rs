#![cfg(test)]

use serde_json::{json, Value};
use two_bot_cutover::settings::SettingsStore;
use two_bot_testsupport::TestDatabase;

use super::*;
use crate::discord_test_common::{MockRest, ScriptedResponse};

const GUILD: &str = "3333";
const CHILD: &str = "COMMUNITY_LIVE_TICK_CHILD";

// Poll::new installs a process-wide OnceLock. Exercise that real publication
// path in a child, never install a reader in the threaded test-suite process.
// Inherit coverage variables; only the child gets fixture-only TLS and no ops URL.
async fn isolated_child(test: &'static str) -> bool {
    if std::env::var(CHILD).as_deref() == Ok(test) {
        return true;
    }
    if std::env::var_os("TWO_TEST_DATABASE_URL").is_none() {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP live community tick: TWO_TEST_DATABASE_URL is not set");
        return false;
    }
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([test, "--exact", "--nocapture"])
            .env(CHILD, test)
            .env("TWO_DATABASE_TLS", "local-only")
            .env_remove(two_bot_core::ops_fetch::OPS_CONFIG_URL_ENV)
            .env_remove(two_bot_core::ops_fetch::OPS_CONFIG_TOKEN_ENV)
            .output()
            .expect("run isolated tick test")
    })
    .await
    .expect("join isolated tick test");
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed"),
        "{test}: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

async fn fixture() -> (TestDatabase, Job) {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("guarded test bootstrap");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create disposable database");
    // The poll opens its own runtime pool, but only against this owned fixture.
    let url = format!("postgres://agent_test:@agent-testdb:5432/{}", db.name());
    two_bot_testsupport::guard_database_url(&url).expect("guard poll fixture URL");
    assert!(crate::settings_jobs::live().is_none(), "fresh child");
    let poll = crate::settings_jobs::job(&url);
    (db, poll)
}

async fn publish(pool: &PgPool, poll: &Job, key: &str, value: Option<Value>) {
    SettingsStore::new(pool)
        .set(GUILD, key, value.clone(), "live-tick-test")
        .await
        .expect("write fixture setting");
    (poll.action)()
        .await
        .expect("publish through real poll job");
    assert_eq!(
        crate::settings_jobs::live()
            .expect("registered poll reader")
            .get(GUILD, key),
        value,
        "poll published the committed row before the production tick"
    );
}

fn state() -> State {
    State {
        capture_started_at: "2026-09-01T00:00:00.000Z".to_owned(),
        classifier_version: "community-v1".to_owned(),
        gates: ScorecardGates {
            enabled: true,
            recommendations_enabled: false,
            correction_cycles: 0,
        },
        inactivity_days: 14,
        scorecard_lane: Mutex::new(()),
        probe_lease: Mutex::new(None),
    }
}

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("synthetic-live-tick-token".to_owned(), Some(mock.origin()))
        .expect("fixture REST executor")
}

// Removing presence_tick's live gate must produce REST requests and a row
// during the disabled tick, not merely change a helper's return value.
#[tokio::test]
async fn presence_tick_obeys_published_disable_and_delete() {
    if !isolated_child(
        "community_jobs::live_tick_tests::presence_tick_obeys_published_disable_and_delete",
    )
    .await
    {
        return;
    }
    let (db, poll) = fixture().await;
    let pool = db.pool();
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"approximate_presence_count": 42})),
            ScriptedResponse::json(200, json!([])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);
    let state = state();
    let now = parse_iso_millis("2026-09-28T01:00:00.000Z").expect("fixed clock");
    publish(pool, &poll, "TWO_PRESENCE_PROBE", Some(json!(0))).await;
    run_once(Kind::PresenceProbe, pool, &rest, GUILD, &state, now)
        .await
        .expect("disabled tick");
    assert!(
        mock.requests().is_empty(),
        "disabled tick makes no REST call"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM presence_probe")
            .fetch_one(pool)
            .await
            .expect("count probe rows"),
        0
    );
    assert_eq!(*state.probe_lease.lock().await, None);

    publish(pool, &poll, "TWO_PRESENCE_PROBE", None).await;
    run_once(Kind::PresenceProbe, pool, &rest, GUILD, &state, now)
        .await
        .expect("deleted override restores boot-enabled tick");
    let rows: Vec<(i32, Option<i32>)> = sqlx::query_as(
        "SELECT approximate_presence_count, bot_floor FROM presence_probe WHERE guild_id=$1",
    )
    .bind(GUILD)
    .fetch_all(pool)
    .await
    .expect("probe rows");
    assert_eq!(rows, vec![(42, Some(0))]);
    assert_eq!(mock.requests().len(), 2, "count read and roster read");
    mock.shutdown().await;
    drop(poll);
    db.close().await.expect("drop disposable database");
}

// Replacing scorecard_once's live gates with state.gates must reserve and
// publish a run on the disabled Monday tick, failing the zero-row assertions.
#[tokio::test]
async fn scorecard_tick_obeys_published_disable_and_delete() {
    if !isolated_child(
        "community_jobs::live_tick_tests::scorecard_tick_obeys_published_disable_and_delete",
    )
    .await
    {
        return;
    }
    let (db, poll) = fixture().await;
    let pool = db.pool();
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rest = executor(&mock);
    let state = state();
    let now = parse_iso_millis("2026-09-28T06:15:00.000Z").expect("eligible Monday");
    publish(pool, &poll, "TWO_COMMUNITY_SCORECARD", Some(json!(0))).await;
    run_once(Kind::Scorecard, pool, &rest, GUILD, &state, now)
        .await
        .expect("disabled tick");
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM community_scorecard_attempts),
                (SELECT COUNT(*) FROM community_scorecard_runs)",
    )
    .fetch_one(pool)
    .await
    .expect("disabled scorecard rows");
    assert_eq!(counts, (0, 0), "no reservation or output while disabled");

    publish(pool, &poll, "TWO_COMMUNITY_SCORECARD", None).await;
    run_once(Kind::Scorecard, pool, &rest, GUILD, &state, now)
        .await
        .expect("deleted override restores boot-enabled tick");
    let attempt: (i32, bool) = sqlx::query_as(
        "SELECT attempts, completed FROM community_scorecard_attempts WHERE guild_id=$1",
    )
    .bind(GUILD)
    .fetch_one(pool)
    .await
    .expect("completed attempt");
    assert_eq!(attempt, (1, true));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM community_scorecard_runs")
            .fetch_one(pool)
            .await
            .expect("count scorecards"),
        1
    );
    assert!(mock.requests().is_empty(), "scorecard never calls Discord");
    mock.shutdown().await;
    drop(poll);
    db.close().await.expect("drop disposable database");
}

async fn persisted_scorecard_tick(
    pool: &PgPool,
    rest: &ActionExecutor,
    state: &State,
    now: &str,
) -> Value {
    run_once(
        Kind::Scorecard,
        pool,
        rest,
        GUILD,
        state,
        parse_iso_millis(now).expect("eligible Monday"),
    )
    .await
    .expect("scorecard tick");
    // Exact generated_at requires a fresh output, not reuse of a completed week.
    let payload: String = sqlx::query_scalar(
        "SELECT scorecard_json FROM community_scorecard_runs WHERE guild_id=$1 AND generated_at=$2",
    )
    .bind(GUILD)
    .bind(now)
    .fetch_one(pool)
    .await
    .expect("fresh persisted scorecard");
    let scorecard: Value = serde_json::from_str(&payload).expect("scorecard JSON");
    assert_eq!(scorecard["coverageState"], "incomplete");
    scorecard
}

// A boot-only recommendation field at ScorecardLoad must lose the live enable.
#[tokio::test]
async fn scorecard_tick_uses_published_recommendations_and_boot_fallback() {
    if !isolated_child(
        "community_jobs::live_tick_tests::scorecard_tick_uses_published_recommendations_and_boot_fallback",
    )
    .await
    {
        return;
    }
    let (db, poll) = fixture().await;
    let pool = db.pool();
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rest = executor(&mock);
    // Boot recommendations off, correction cycles zero.
    let mut state = state();
    // A Monday 06:15 boot misses all of the first week and the opening hours
    // of the second: neither tick can claim complete stream coverage.
    state.capture_started_at = "2026-09-28T06:15:00.000Z".to_owned();
    for (now, stored, recommendations) in [
        ("2026-09-28T06:15:00.000Z", Some(json!(1)), true),
        ("2026-10-05T06:15:00.000Z", None, false),
    ] {
        publish(pool, &poll, "TWO_COMMUNITY_RECOMMENDATIONS", stored).await;
        let scorecard = persisted_scorecard_tick(pool, &rest, &state, now).await;
        assert_eq!(
            scorecard["recommendationsEnabled"], recommendations,
            "{now}"
        );
        assert_eq!(scorecard["killSwitchActive"], false, "{now}");
    }
    assert!(mock.requests().is_empty(), "scorecard never calls Discord");
    mock.shutdown().await;
    drop(poll);
    db.close().await.expect("drop disposable database");
}

// A boot-only correction field at ScorecardLoad must miss the live kill switch.
#[tokio::test]
async fn scorecard_tick_uses_published_corrections_and_boot_fallback() {
    if !isolated_child(
        "community_jobs::live_tick_tests::scorecard_tick_uses_published_corrections_and_boot_fallback",
    )
    .await
    {
        return;
    }
    let (db, poll) = fixture().await;
    let pool = db.pool();
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rest = executor(&mock);
    let mut state = state();
    state.gates.recommendations_enabled = true;
    state.capture_started_at = "2026-09-28T06:15:00.000Z".to_owned();
    // This late boot leaves both weeks incompletely covered, so two corrections
    // trip the kill switch. Boot zero and deletion must leave recommendations on.
    for (now, stored, kill_switch) in [
        ("2026-09-28T06:15:00.000Z", Some(json!(2)), true),
        ("2026-10-05T06:15:00.000Z", None, false),
    ] {
        publish(pool, &poll, "TWO_COMMUNITY_CORRECTION_CYCLES", stored).await;
        let scorecard = persisted_scorecard_tick(pool, &rest, &state, now).await;
        assert_eq!(scorecard["killSwitchActive"], kill_switch, "{now}");
        assert_eq!(scorecard["recommendationsEnabled"], !kill_switch, "{now}");
    }
    assert!(mock.requests().is_empty(), "scorecard never calls Discord");
    mock.shutdown().await;
    drop(poll);
    db.close().await.expect("drop disposable database");
}

// Replacing inactivity_tick's live horizon with state.inactivity_days (14)
// flags this 20-day-quiet member on the first tick instead of waiting for 7.
#[tokio::test]
async fn inactivity_tick_uses_published_horizon_and_boot_fallback() {
    if !isolated_child(
        "community_jobs::live_tick_tests::inactivity_tick_uses_published_horizon_and_boot_fallback",
    )
    .await
    {
        return;
    }
    let (db, poll) = fixture().await;
    let pool = db.pool();
    let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
    let rest = executor(&mock);
    let state = state();
    sqlx::query(
        "INSERT INTO members (guild_id, member_id, joined_at, last_active_at, is_bot)
         VALUES ($1, '7000', '2026-08-01'::timestamptz, '2026-09-08T01:00:00Z'::timestamptz, FALSE)",
    )
    .bind(GUILD)
    .execute(pool)
    .await
    .expect("seed quiet member");
    let now = parse_iso_millis("2026-09-28T01:00:00.000Z").expect("fixed clock");
    publish(pool, &poll, "TWO_INACTIVITY_DAYS", Some(json!(30))).await;
    run_once(Kind::Inactivity, pool, &rest, GUILD, &state, now)
        .await
        .expect("30-day sweep");
    let first: (bool, i64) = sqlx::query_as(
        "SELECT inactive_flagged_at IS NULL,
                (SELECT COUNT(*) FROM events WHERE event_type='member_inactive')
         FROM members WHERE guild_id=$1 AND member_id='7000'",
    )
    .bind(GUILD)
    .fetch_one(pool)
    .await
    .expect("unflagged member");
    assert_eq!(first, (true, 0), "stored 30 overrides boot 14");

    publish(pool, &poll, "TWO_INACTIVITY_DAYS", Some(json!(7))).await;
    run_once(Kind::Inactivity, pool, &rest, GUILD, &state, now)
        .await
        .expect("7-day sweep");
    let thresholds: Vec<String> = sqlx::query_scalar(
        "SELECT metadata FROM events WHERE event_type='member_inactive' AND member_id='7000'",
    )
    .fetch_all(pool)
    .await
    .expect("flag event");
    assert_eq!(thresholds, vec!["{\"thresholdDays\":7}"]);
    let flagged: bool = sqlx::query_scalar(
        "SELECT inactive_flagged_at=$2::timestamptz FROM members WHERE guild_id=$1 AND member_id='7000'",
    )
    .bind(GUILD)
    .bind("2026-09-28T01:00:00.000Z")
    .fetch_one(pool)
    .await
    .expect("flag projection");
    assert!(flagged);

    sqlx::query(
        "INSERT INTO members (guild_id, member_id, joined_at, is_bot)
         VALUES ($1, '7001', '2026-09-08T01:00:00Z'::timestamptz, FALSE)",
    )
    .bind(GUILD)
    .execute(pool)
    .await
    .expect("seed boot-fallback member");
    publish(pool, &poll, "TWO_INACTIVITY_DAYS", None).await;
    run_once(Kind::Inactivity, pool, &rest, GUILD, &state, now)
        .await
        .expect("deleted horizon restores boot 14");
    let thresholds: Vec<String> = sqlx::query_scalar(
        "SELECT metadata FROM events WHERE event_type='member_inactive' AND member_id='7001'",
    )
    .fetch_all(pool)
    .await
    .expect("boot horizon flag");
    assert_eq!(thresholds, vec!["{\"thresholdDays\":14}"]);
    assert!(mock.requests().is_empty(), "inactivity never calls Discord");
    mock.shutdown().await;
    drop(poll);
    db.close().await.expect("drop disposable database");
}
