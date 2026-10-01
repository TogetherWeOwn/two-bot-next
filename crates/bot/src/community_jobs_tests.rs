use std::collections::HashMap;

use serde_json::{json, Value};
use two_bot_testsupport::TestDatabase;

use super::*;

#[allow(dead_code)]
#[path = "../../discord/tests/common/mod.rs"]
mod common;

use common::{MockRest, ScriptedResponse};

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("synthetic-job-test-token".to_owned(), Some(mock.origin())).unwrap()
}

fn member(id: u64, bot: bool) -> Value {
    json!({"user": {"id": id.to_string(), "bot": bot}, "roles": []})
}

fn enabled_gates() -> ScorecardGates {
    ScorecardGates {
        enabled: true,
        recommendations_enabled: false,
        correction_cycles: 0,
    }
}

fn fresh_state(gates: ScorecardGates, inactivity_days: u64) -> State {
    State {
        capture_started_at: "2026-09-01T00:00:00.000Z".to_owned(),
        classifier_version: "community-v1".to_owned(),
        gates,
        inactivity_days,
        last_attempted_week: Mutex::new(None),
    }
}

fn parked_names(parked: &[Parked]) -> Vec<&'static str> {
    parked.iter().map(|p| p.name).collect()
}

#[test]
fn gates_follow_legacy_env_semantics() {
    let (gates, parked) = resolve_gates(&HashMap::new());
    assert!(gates.presence, "presence probe is on by default");
    assert!(gates.scorecard.is_none(), "scorecard is off by default");
    assert_eq!(gates.inactivity_days, Some(14));
    assert_eq!(parked_names(&parked), ["community_scorecard"]);

    let mut vars = HashMap::from([
        ("TWO_PRESENCE_PROBE".to_owned(), "0".to_owned()),
        ("TWO_COMMUNITY_SCORECARD".to_owned(), "1".to_owned()),
        ("TWO_INACTIVITY_DAYS".to_owned(), "30".to_owned()),
    ]);
    let (gates, parked) = resolve_gates(&vars);
    assert!(!gates.presence);
    assert!(gates.scorecard.expect("scorecard enabled").enabled);
    assert_eq!(gates.inactivity_days, Some(30));
    assert_eq!(parked_names(&parked), ["presence_probe"]);

    vars.insert("TWO_INACTIVITY_DAYS".to_owned(), "many".to_owned());
    let (gates, parked) = resolve_gates(&vars);
    assert_eq!(gates.inactivity_days, None);
    assert!(
        parked
            .iter()
            .any(|p| p.name == "inactivity" && p.reason == "invalid_config"),
        "unparsable TWO_INACTIVITY_DAYS parks the sweep"
    );

    vars.insert("TWO_INACTIVITY_DAYS".to_owned(), "14".to_owned());
    vars.insert(
        "TWO_COMMUNITY_CORRECTION_CYCLES".to_owned(),
        "bogus".to_owned(),
    );
    let (gates, parked) = resolve_gates(&vars);
    assert!(gates.scorecard.is_none());
    assert!(
        parked
            .iter()
            .any(|p| p.name == "community_scorecard" && p.reason == "invalid_config"),
        "bad scorecard gate value parks the scorecard"
    );
}

#[tokio::test]
async fn scorecard_attempt_is_once_per_monday_per_process() {
    // 2026-09-28 is a Monday; the window opens at 06:15 UTC.
    let monday = parse_iso_millis("2026-09-28T06:15:00.000Z").unwrap();
    let state = fresh_state(enabled_gates(), 14);
    assert_eq!(
        consume_attempt(&state, monday).await.as_deref(),
        Some("2026-09-28")
    );
    assert_eq!(
        consume_attempt(&state, monday + 60_000).await,
        None,
        "same process consumes a Monday once"
    );
    // A restart rebuilds State: the tick refires and the runs-table claim
    // dedupes the row (asserted in the integration test below).
    let restarted = fresh_state(enabled_gates(), 14);
    assert_eq!(
        consume_attempt(&restarted, monday + 3_600_000)
            .await
            .as_deref(),
        Some("2026-09-28")
    );
    assert_eq!(
        consume_attempt(&restarted, monday + 7 * 86_400_000)
            .await
            .as_deref(),
        Some("2026-10-05"),
        "the next Monday is a fresh key"
    );
    // Outside the window nothing is consumed.
    let fresh = fresh_state(enabled_gates(), 14);
    assert_eq!(
        consume_attempt(&fresh, monday - 1_000).await,
        None,
        "06:14:59 is before the window"
    );
    assert_eq!(
        consume_attempt(
            &fresh,
            parse_iso_millis("2026-09-29T06:15:00.000Z").unwrap()
        )
        .await,
        None,
        "Tuesday never fires"
    );
}

/// Real migrations + mock REST: one tick of each job writes its rows, the
/// scorecard is exactly-once per Monday across a simulated restart, the 24 h
/// bot-floor listing is honored, and the inactivity sweep flags once.
#[tokio::test]
async fn community_ticks_write_rows_and_stay_gated() {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP community job integration: TWO_TEST_DATABASE_URL is not set");
        return;
    };
    let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated agent-testdb fixture");
    let pool = fixture.pool().clone();
    let guild = "3333";

    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"approximate_presence_count": 42})),
            ScriptedResponse::json(
                200,
                json!([member(5000, false), member(5001, true), member(5002, true)]),
            ),
            ScriptedResponse::json(200, json!({"approximate_presence_count": 43})),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);
    let state = fresh_state(enabled_gates(), 14);

    // First probe: no floor observed yet, so the roster listing runs and the
    // row carries the counted floor. Second probe an hour later: the 24 h
    // budget holds, no listing, floor column stays NULL ("not rescanned").
    let t0 = parse_iso_millis("2026-09-28T01:00:00.000Z").unwrap();
    run_once(Kind::PresenceProbe, &pool, &rest, guild, &state, t0)
        .await
        .unwrap();
    run_once(
        Kind::PresenceProbe,
        &pool,
        &rest,
        guild,
        &state,
        t0 + 3_600_000,
    )
    .await
    .unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 3, "counts, one member listing, counts");
    assert!(
        requests[1].path.contains("/members?limit=1000"),
        "{}",
        requests[1].path
    );
    drop(requests);
    let rows: Vec<(i32, Option<i32>)> = sqlx::query_as(
        "SELECT approximate_presence_count, bot_floor FROM presence_probe
          WHERE guild_id=$1 ORDER BY observed_at",
    )
    .bind(guild)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows, vec![(42, Some(2)), (43, None)]);

    // Monday 06:15 UTC: six stream heartbeats then one run row. A second tick
    // in the same process is consumed; a fresh State (restart) refires the
    // tick but the runs-table idempotency key dedupes the row.
    let monday = parse_iso_millis("2026-09-28T06:15:00.000Z").unwrap();
    run_once(Kind::Scorecard, &pool, &rest, guild, &state, monday)
        .await
        .unwrap();
    run_once(
        Kind::Scorecard,
        &pool,
        &rest,
        guild,
        &state,
        monday + 60_000,
    )
    .await
    .unwrap();
    let restarted = fresh_state(enabled_gates(), 14);
    run_once(
        Kind::Scorecard,
        &pool,
        &rest,
        guild,
        &restarted,
        monday + 120_000,
    )
    .await
    .unwrap();
    let runs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM community_scorecard_runs WHERE guild_id=$1")
            .bind(guild)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(runs, 1, "one run row across retry and restart");
    let beats: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM community_stream_heartbeats WHERE guild_id=$1")
            .bind(guild)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(beats, 6, "coverage marked for every fact stream");
    run_once(
        Kind::Scorecard,
        &pool,
        &rest,
        guild,
        &restarted,
        parse_iso_millis("2026-09-30T06:15:00.000Z").unwrap(),
    )
    .await
    .unwrap();
    let runs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM community_scorecard_runs WHERE guild_id=$1")
            .bind(guild)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(runs, 1, "out-of-window ticks write nothing");

    // The hourly sweep flags the stale member once: `inactive_flagged_at`
    // stamps, one `member_inactive` event lands, and a repeat sweep is a
    // no-op (read-only — the outcome can never feed a send path).
    sqlx::query(
        "INSERT INTO members (guild_id, member_id, joined_at, last_active_at, is_bot, left_at)
         VALUES ($1, $2, $3::timestamptz, $4::timestamptz, FALSE, NULL)",
    )
    .bind(guild)
    .bind("7000")
    .bind("2026-08-01T00:00:00.000Z")
    .bind("2026-08-02T00:00:00.000Z")
    .execute(&pool)
    .await
    .unwrap();
    run_once(Kind::Inactivity, &pool, &rest, guild, &state, t0)
        .await
        .unwrap();
    run_once(
        Kind::Inactivity,
        &pool,
        &rest,
        guild,
        &state,
        t0 + 3_600_000,
    )
    .await
    .unwrap();
    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE event_type='member_inactive' AND guild_id=$1",
    )
    .bind(guild)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 1, "one flag event across both sweeps");
    let flagged: Option<String> = sqlx::query_scalar(
        "SELECT to_char(inactive_flagged_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
           FROM members WHERE guild_id=$1 AND member_id='7000'",
    )
    .bind(guild)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(flagged.as_deref(), Some("2026-09-28T01:00:00.000Z"));

    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}
