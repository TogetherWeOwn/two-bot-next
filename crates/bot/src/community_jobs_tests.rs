#![cfg(test)]

use std::collections::HashMap;

use serde_json::{json, Value};
use two_bot_testsupport::TestDatabase;

use super::*;

use crate::discord_test_common::{MockRest, ScriptedResponse};

#[path = "community_scorecard_retry_db_tests.rs"]
mod retry_db;

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("synthetic-job-test-token".to_owned(), Some(mock.origin())).unwrap()
}

/// TOG-15758 decision: the community jobs sit outside the live-identity
/// capability fence because none of them writes to Discord. The presence probe
/// only reads the guild and roster, the scorecard and the inactivity sweep touch
/// Postgres alone. This pin fails the moment one of them (or the shared reads
/// they call) gains a Discord write verb: bind that job to a `LiveCapability`
/// in `activation::BootActivation` before it ships, as the ticker and the feed
/// poller are.
#[test]
fn community_jobs_have_no_discord_write_path() {
    const WRITE_VERBS: [&str; 17] = [
        ".post_",
        ".delete_",
        ".put_",
        ".edit_",
        ".execute_",
        ".set_",
        ".ban(",
        ".unban(",
        ".kick",
        ".timeout_",
        ".purge",
        ".finish_interaction",
        ".sync_guild_commands",
        ".send_request",
        ".call_once_raw",
        ".recover_message",
        ".member_role_ids",
    ];
    for (file, source) in [
        ("community_jobs.rs", include_str!("community_jobs.rs")),
        ("website_jobs.rs", include_str!("website_jobs.rs")),
    ] {
        for verb in WRITE_VERBS {
            assert!(
                !source.contains(verb),
                "{file} calls `{verb}`: a community job with a Discord write path must be \
                 bound to a LiveCapability before it ships"
            );
        }
    }
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
        scorecard_lane: Mutex::new(()),
        probe_lease: Mutex::new(None),
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

#[test]
fn parked_reason_taxonomy_selects_warn_vs_info() {
    // Contract: `register` logs WARN exactly when a parked job's `reason`
    // is `"invalid_config"` and INFO otherwise (its match on the reason).
    // This pins the reason every gate state produces, so a future refactor
    // cannot silently move a misconfiguration onto the info path — or a
    // plain disable onto warn — without failing here.
    let reasons_for = |parked: &[Parked], name: &str| {
        parked
            .iter()
            .filter(|p| p.name == name)
            .map(|p| p.reason)
            .collect::<Vec<_>>()
    };

    // Default env: the scorecard is off (a plain disable) and the sweep
    // runs at the legacy default.
    let (gates, parked) = resolve_gates(&HashMap::new());
    assert_eq!(gates.inactivity_days, Some(14));
    assert_eq!(
        parked
            .iter()
            .map(|p| (p.name, p.reason))
            .collect::<Vec<_>>(),
        [("community_scorecard", "disabled")]
    );

    // Presence explicitly off is a disable (info), not a config error.
    let vars = HashMap::from([("TWO_PRESENCE_PROBE".to_owned(), "0".to_owned())]);
    let (gates, parked) = resolve_gates(&vars);
    assert!(!gates.presence);
    assert_eq!(reasons_for(&parked, "presence_probe"), ["disabled"]);

    // A bad scorecard value is a config error (warn).
    let vars = HashMap::from([
        ("TWO_COMMUNITY_SCORECARD".to_owned(), "1".to_owned()),
        (
            "TWO_COMMUNITY_CORRECTION_CYCLES".to_owned(),
            "bogus".to_owned(),
        ),
    ]);
    let (gates, parked) = resolve_gates(&vars);
    assert!(gates.scorecard.is_none());
    assert_eq!(
        reasons_for(&parked, "community_scorecard"),
        ["invalid_config"]
    );

    // Every non-numeric TWO_INACTIVITY_DAYS shape parks the sweep as
    // invalid_config (warn): never "disabled", never the raw value.
    for raw in ["many", "", "-1", "14.5", "18446744073709551616"] {
        let vars = HashMap::from([("TWO_INACTIVITY_DAYS".to_owned(), raw.to_owned())]);
        let (gates, parked) = resolve_gates(&vars);
        assert_eq!(gates.inactivity_days, None, "days {raw:?} parks the sweep");
        assert_eq!(
            reasons_for(&parked, "inactivity"),
            ["invalid_config"],
            "days {raw:?} must select the warn path"
        );
    }

    // Parseable shapes stay green — including the saturation extreme, which
    // `inactivity_cutoff_ms` pins to the `i64::MIN`-bounded floor, and
    // surrounding whitespace, which the parser trims.
    for (raw, days) in [("0", 0), (" 30 ", 30), ("18446744073709551615", u64::MAX)] {
        let vars = HashMap::from([("TWO_INACTIVITY_DAYS".to_owned(), raw.to_owned())]);
        let (gates, parked) = resolve_gates(&vars);
        assert_eq!(gates.inactivity_days, Some(days), "days {raw:?} parses");
        assert!(
            reasons_for(&parked, "inactivity").is_empty(),
            "days {raw:?} parks nothing"
        );
    }
}

use scorecard_retry::{decide, AttemptState, Decision, RETRY_DELAY_MS};

fn at(iso: &str) -> i64 {
    parse_iso_millis(iso).unwrap()
}

fn reserve(state: &mut AttemptState, now: i64) -> bool {
    match decide(state, now) {
        Decision::Attempt(next) => {
            *state = next;
            true
        }
        Decision::Wait | Decision::Skip => false,
    }
}

// Legacy retry cases use a fake clock and synthetic coverage/scoring failures,
// without timers, credentials, a database or a network.
fn transient_then_success(fail_coverage: bool) {
    let monday = at("2026-09-07T06:15:00.000Z");
    let mut state = AttemptState::default();
    let mut coverage_calls = 0;
    let mut scoring_calls = 0;
    let mut published = 0;
    for (now, succeeds) in [
        (monday, false),
        (monday + RETRY_DELAY_MS - 1, true),
        (monday + RETRY_DELAY_MS, true),
        (at("2026-09-07T06:59:00.000Z"), true),
        (at("2026-09-07T06:59:00.000Z"), true),
    ] {
        if !reserve(&mut state, now) {
            continue;
        }
        coverage_calls += 1;
        if !succeeds && fail_coverage {
            continue;
        }
        scoring_calls += 1;
        if succeeds {
            published += 1;
            state.completed = true;
        }
    }
    assert_eq!(coverage_calls, 2);
    assert_eq!(scoring_calls, if fail_coverage { 1 } else { 2 });
    assert_eq!(published, 1);
    assert_eq!(state.attempts, 2);
    assert_eq!(state.week_key, "2026-09-07");
    assert_eq!(
        previous_closed_week(monday),
        (
            "2026-08-31T00:00:00.000Z".to_owned(),
            "2026-09-07T00:00:00.000Z".to_owned(),
        )
    );
}

#[test]
fn scorecard_retries_transient_coverage_then_suppresses_duplicates() {
    transient_then_success(true);
}

#[test]
fn scorecard_retries_transient_scoring_then_suppresses_duplicates() {
    transient_then_success(false);
}

#[test]
fn scorecard_three_spaced_failures_and_new_monday_budget() {
    let monday = at("2026-09-07T06:15:00.000Z");
    let mut state = AttemptState::default();
    for minute in 15..=59 {
        for _ in 0..2 {
            reserve(&mut state, monday + (minute - 15) * 60_000);
            assert_eq!(
                state.attempts,
                if minute < 20 {
                    1
                } else if minute < 25 {
                    2
                } else {
                    3
                }
            );
        }
    }
    assert_eq!(decide(&state, monday + 600_000), Decision::Skip);
    assert!(reserve(&mut state, monday + 7 * 86_400_000));
    assert_eq!(state.attempts, 1);
    assert_eq!(state.week_key, "2026-09-14");
}

#[test]
fn scorecard_retry_never_catches_up_outside_monday_window() {
    let mut state = AttemptState::default();
    assert_eq!(
        decide(&state, at("2026-09-07T06:14:59.999Z")),
        Decision::Skip
    );
    assert!(reserve(&mut state, at("2026-09-07T06:59:59.999Z")));
    for now in [
        "2026-09-07T07:00:00.000Z",
        "2026-09-07T07:04:00.000Z",
        "2026-09-08T06:20:00.000Z",
        "2026-09-14T06:14:00.000Z",
    ] {
        assert_eq!(decide(&state, at(now)), Decision::Skip);
        assert_eq!(state.attempts, 1);
    }
    assert!(reserve(&mut state, at("2026-09-14T06:15:00.000Z")));
    assert_eq!(state.attempts, 1);
}

#[tokio::test]
async fn scorecard_eligible_ticks_do_not_overlap_even_across_weeks() {
    let state = fresh_state(enabled_gates(), 14);
    let _in_flight = state.scorecard_lane.lock().await;
    // A lazy pool deliberately has no listener. The busy lane must return
    // before any database access, just as the supervisor skips busy deadlines.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@agent-testdb:5432/agent_test")
        .unwrap();
    for now in [
        "2026-09-07T06:15:00.000Z",
        "2026-09-07T06:20:00.000Z",
        "2026-09-14T06:15:00.000Z",
    ] {
        scorecard_once(&pool, "guild-a", &state, at(now))
            .await
            .unwrap();
    }
    pool.close().await;
}

#[tokio::test(start_paused = true)]
async fn scorecard_stop_cancels_future_retry_ticks() {
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use tokio::sync::watch;

    let monday = at("2026-09-07T06:15:00.000Z");
    let clock = Arc::new(AtomicI64::new(monday));
    let attempts = Arc::new(AtomicUsize::new(0));
    let saved = Arc::new(Mutex::new(AttemptState::default()));
    let action = {
        let clock = clock.clone();
        let attempts = attempts.clone();
        let saved = saved.clone();
        Arc::new(move || {
            let clock = clock.clone();
            let attempts = attempts.clone();
            let saved = saved.clone();
            Box::pin(async move {
                if reserve(&mut *saved.lock().await, clock.load(Ordering::SeqCst)) {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    return Err(ErrorClass::Database);
                }
                Ok(())
            }) as jobs::JobFuture
        })
    };
    let (stop, shutdown) = watch::channel(false);
    let task = tokio::spawn(jobs::supervise(
        vec![Job {
            name: "community_scorecard",
            cadence: cadence(Kind::Scorecard),
            startup_jitter: Duration::ZERO,
            timeout: Duration::from_secs(120),
            action,
        }],
        jobs::statuses(&["community_scorecard"], false),
        shutdown,
    ));
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    stop.send(true).unwrap();
    task.await.unwrap();
    clock.store(monday + RETRY_DELAY_MS, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(300)).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[test]
fn scorecard_reloaded_reservation_preserves_backoff_and_crash_budget() {
    let monday = at("2026-09-07T06:15:00.000Z");
    let mut persisted = AttemptState::default();
    for attempt in 1..=3 {
        let now = monday + (i64::from(attempt) - 1) * RETRY_DELAY_MS;
        assert!(reserve(&mut persisted, now));
        assert_eq!(persisted.attempts, attempt);
        let restarted = persisted.clone();
        assert_eq!(
            decide(&restarted, now + RETRY_DELAY_MS - 1),
            if attempt == 3 {
                Decision::Skip
            } else {
                Decision::Wait
            }
        );
    }
    assert_eq!(
        decide(&persisted.clone(), monday + 3 * RETRY_DELAY_MS),
        Decision::Skip
    );
}

#[tokio::test]
async fn truncated_presence_scan_persists_and_waits_24_hours_across_restart() {
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
    let pool = fixture.pool();
    let guild = "3333";
    let counts = || ScriptedResponse::json(200, json!({"approximate_presence_count": 42}));
    let mut responses = vec![counts()];
    for page in 0..11 {
        let members: Vec<_> = (page * 1000 + 1..=page * 1000 + 1000)
            .map(|id| member(id, id % 10 == 0))
            .collect();
        responses.push(ScriptedResponse::json(200, json!(members)));
    }
    responses.extend([
        counts(), // Hourly tick, no scan.
        counts(), // One millisecond before 24 h, no scan.
        counts(), // Exactly 24 h: a new complete scan.
        ScriptedResponse::json(200, json!([member(1, true)])),
    ]);
    let mock = MockRest::start(responses, ScriptedResponse::status(500)).await;
    let rest = executor(&mock);
    let state = fresh_state(enabled_gates(), 14);
    let now = parse_iso_millis("2026-10-01T00:00:00.000Z").unwrap();
    presence_tick(pool, &rest, guild, &state, now)
        .await
        .unwrap();
    assert_eq!(
        mock.requests().len(),
        12,
        "one counts read + eleven roster requests"
    );
    // A fresh executor has no in-memory cadence state: the persisted bit gates it.
    let restarted = executor(&mock);
    presence_tick(pool, &restarted, guild, &state, now + 3_600_000)
        .await
        .unwrap();
    presence_tick(
        pool,
        &restarted,
        guild,
        &state,
        now + BOT_FLOOR_MAX_AGE_MS as i64 - 1,
    )
    .await
    .unwrap();
    assert_eq!(mock.requests().len(), 14, "no premature roster rescan");
    presence_tick(
        pool,
        &restarted,
        guild,
        &state,
        now + BOT_FLOOR_MAX_AGE_MS as i64,
    )
    .await
    .unwrap();
    assert_eq!(mock.requests().len(), 16, "24 h boundary rescans");
    let rows: Vec<(i32, Option<i32>, bool)> = sqlx::query_as(
        "SELECT approximate_presence_count, bot_floor, bot_floor_scan_truncated
           FROM presence_probe WHERE guild_id=$1 ORDER BY observed_at",
    )
    .bind(guild)
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            (42, None, true),
            (42, None, false),
            (42, None, false),
            (42, Some(1), false)
        ]
    );
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

/// Overlap guard (TOG-12142): a concurrent trigger skips before any REST
/// call, the in-flight cycle still records its row, and the lease clears so
/// the next tick runs normally.
#[tokio::test]
async fn overlapping_probe_trigger_skips_and_inflight_cycle_records() {
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
    let counts = || ScriptedResponse::json(200, json!({"approximate_presence_count": 42}));
    let members = vec![member(5000, false), member(5001, true), member(5002, true)];
    // The roster page is gated: the first cycle parks inside the scan holding
    // the lease while the concurrent trigger fires. The default keeps serving
    // counts so the post-skip tick proves the normal cycle is unaffected.
    let (mock, gate) = MockRest::start_gated(
        vec![counts(), ScriptedResponse::json(200, json!(members))],
        counts(),
    )
    .await;
    let rest = executor(&mock);
    let state = Arc::new(fresh_state(enabled_gates(), 14));
    let t0 = parse_iso_millis("2026-09-28T01:00:00.000Z").unwrap();

    let first = {
        let pool = pool.clone();
        let rest = rest.clone();
        let state = Arc::clone(&state);
        tokio::spawn(async move { presence_tick(&pool, &rest, guild, &state, t0).await })
    };
    tokio::time::timeout(Duration::from_secs(5), gate.wait_for_request())
        .await
        .expect("the first cycle must reach the gated roster scan");
    assert!(
        !first.is_finished(),
        "the roster scan is still withheld: the lease is held"
    );
    // Concurrent trigger while the first cycle holds the lease: skips before
    // any REST call and writes no row.
    presence_tick(&pool, &rest, guild, &state, t0)
        .await
        .unwrap();
    assert_eq!(
        mock.requests().len(),
        2,
        "counts plus the gated roster page; the skipped trigger calls nothing"
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM presence_probe WHERE guild_id=$1")
        .bind(guild)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "a skipped trigger writes nothing");
    gate.release();
    tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("the first cycle finishes after release")
        .unwrap()
        .unwrap();
    assert!(
        state.probe_lease.lock().await.is_none(),
        "the lease clears when the cycle finishes"
    );
    // Normal cycle unaffected: an hour later the tick runs and records.
    presence_tick(&pool, &rest, guild, &state, t0 + 3_600_000)
        .await
        .unwrap();
    let rows: Vec<(i32, Option<i32>)> = sqlx::query_as(
        "SELECT approximate_presence_count, bot_floor FROM presence_probe
          WHERE guild_id=$1 ORDER BY observed_at",
    )
    .bind(guild)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows, vec![(42, Some(2)), (42, None)]);
    mock.shutdown().await;
    fixture.close().await.unwrap();
}

/// Overlap guard (TOG-12142): a lease older than `PRESENCE_PROBE_LEASE_MS`
/// is presumed dead and taken over on the spot, while a fresh holder still
/// makes the next trigger skip.
#[tokio::test]
async fn stale_probe_lease_is_taken_over() {
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
            ScriptedResponse::json(200, json!([member(5000, false)])),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = executor(&mock);
    let state = fresh_state(enabled_gates(), 14);
    let t0 = parse_iso_millis("2026-09-28T01:00:00.000Z").unwrap();
    // A holder from before the lease window is dead: this trigger runs.
    *state.probe_lease.lock().await = Some(t0 - PRESENCE_PROBE_LEASE_MS as i64 - 1);
    presence_tick(&pool, &rest, guild, &state, t0)
        .await
        .unwrap();
    let rows: Vec<(i32, Option<i32>)> = sqlx::query_as(
        "SELECT approximate_presence_count, bot_floor FROM presence_probe
          WHERE guild_id=$1 ORDER BY observed_at",
    )
    .bind(guild)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows, vec![(42, Some(0))]);
    assert!(
        state.probe_lease.lock().await.is_none(),
        "the lease clears after the takeover cycle"
    );
    // A fresh holder blocks the next trigger: skip, no REST, no row.
    *state.probe_lease.lock().await = Some(t0);
    presence_tick(&pool, &rest, guild, &state, t0)
        .await
        .unwrap();
    assert_eq!(
        mock.requests().len(),
        2,
        "a concurrent trigger calls nothing"
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM presence_probe WHERE guild_id=$1")
        .bind(guild)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "a skipped trigger writes nothing");
    mock.shutdown().await;
    fixture.close().await.unwrap();
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
    let (path, query) = requests[1].path.split_once('?').expect("roster query");
    assert_eq!(path, "/api/v10/guilds/3333/members");
    let params: HashMap<_, _> = query
        .split('&')
        .map(|pair| pair.split_once('=').expect("query parameter"))
        .collect();
    assert_eq!(params, HashMap::from([("after", "0"), ("limit", "1000")]));
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

    // Monday 06:15 UTC: six stream heartbeats then one run row. Completion
    // suppresses later ticks, including with a fresh process State.
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
