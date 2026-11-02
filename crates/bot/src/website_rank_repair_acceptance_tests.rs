//! Acceptance: rank repair revalidates eligibility and publishes observed post-repair roles.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the acceptance branch, keep
//! every test here verbatim, and make the RED tests pass by changing
//! production code only (the rank tick's repair path). Do not edit the
//! fixtures, the mock universe or the assertions: they are the spec. If a test
//! is wrong, send it back to the test author; do not fix it in the Builder PR.
//!
//! The defect under test: the tick captures the roster once, plans missing
//! lower-rung grants from that capture, applies the plan against current
//! Discord state, then publishes the old roster plus optimistic insertions.
//! A higher rank removed after the capture still authorizes its lower-rung
//! grant, and the removed rank is published as still current. The fix narrows
//! that observed-staleness window in three places:
//!
//! 1. Re-read and validate each affected member's current rank holdings before
//!    granting, and recompute the plan from observed eligibility.
//! 2. Refuse grants on missing, wrong, incomplete or unreadable evidence
//!    instead of treating it as an empty membership.
//! 3. After repair, refresh the affected members and rebuild the published
//!    snapshot from authoritative observed roles, never optimistic insertion.
//!
//! Suite map (all offline: disposable test database plus the scripted REST
//! double; no live Discord):
//!
//! - RED `higher_rank_removed_before_grant_is_not_repaired`: the member read
//!   observes no holdings, so no PUT lands and the observed-nested roster
//!   publishes with the member unranked.
//! - RED `departed_member_is_skipped_and_remainder_publishes`: the member read
//!   is 404, so no PUT lands and the observed remainder publishes.
//! - RED `failed_member_reread_fails_the_tick_closed`: a 500 member read is
//!   unproven evidence, so the whole tick fails closed with no PUT and no
//!   publication (it must not read as an empty membership).
//! - RED `wrong_identity_member_evidence_fails_the_tick_closed`: a member read
//!   bound to a different user id is unproven evidence with the same closed
//!   outcome. The revalidation read must bind evidence to the requested
//!   member; a roles-only read cannot see this fault.
//! - RED `incomplete_member_evidence_fails_the_tick_closed`: a member read
//!   with no roles field is unproven evidence with the same closed outcome.
//! - RED `role_removed_between_grants_skips_the_second_grant`: the second
//!   affected member loses its higher rank after the first PUT lands, so only
//!   the still-eligible grant lands and publication matches observed truth.
//!   Batch validation before the first grant cannot see this change.
//! - RED `externally_added_rank_between_grant_and_publish_is_observed`: a
//!   higher rank granted out of band after the repair PUT is part of the
//!   observed post-repair roles, so publication reports it instead of the
//!   optimistic lower rank.
//! - RED `failed_post_repair_verification_keeps_prior_snapshot`: the grant
//!   lands but the post-repair member read fails, so the tick errors and the
//!   prior published snapshot is byte-identical (no optimistic publication).
//! - RED `non_sticking_grant_keeps_prior_snapshot`: the grant PUT is accepted
//!   but the post-repair member read still shows the missing rung, so the
//!   tick errors and the prior published snapshot is byte-identical.
//! - GREEN `valid_repair_still_heals_and_publishes`: the ordinary repair path
//!   is unchanged (one PUT with the exact audit reason, member published).
//! - GREEN `already_nested_roster_publishes_without_puts`.
//! - GREEN `over_cap_repair_still_fails_closed`: 26 planned grants keep the
//!   existing 25-grant ceiling refusal.
//! - GREEN `refused_identity_still_sends_no_put`.
//! - GREEN `invalid_ladder_still_fails_closed`.
//! - GREEN `shutdown_tick_sends_no_requests`.
//!
//! Mock universe. The route-aware double models one Discord truth with two
//! read channels, mirroring how list endpoints lag point reads:
//!
//! - `GET /guilds/{g}/members?...` replays the tick's initial roster capture
//!   on every call, frozen. Revalidation MUST read each affected member
//!   (`GET /guilds/{g}/members/{id}`); a roster replay is never revalidation
//!   evidence and the tests fail a fix that treats it as such.
//! - `GET /guilds/{g}/members/{id}` observes current truth: scripted
//!   mid-tick changes, applied PUTs (when they stick), and scripted evidence
//!   faults (status, wrong identity, missing roles).
//! - PUT records the grant, notes whether the target still held a justifying
//!   higher rung at that instant, optionally sticks, then runs the scripted
//!   hook once (a mid-tick external change).
//!
//! Served routes: the roster capture, the guild object, `users/@me`, single
//! member reads (including the bot), and role PUTs. Anything else panics:
//! an unknown route is a non-conforming fix shape, not a 500 to absorb.
//!
//! Accepted fix shapes. Pre-grant evidence that proves ineligibility (removed
//! rank observed with 200, departed with 404) drops that member from the
//! plan; the tick publishes the observed state when it is nested. Pre-grant
//! evidence that proves nothing (500, wrong identity, missing roles) fails
//! the whole tick closed: no PUT at all, no publication, any error. Grants
//! are validated per grant, not once per batch. Post-repair state comes from
//! fresh member reads; unverifiable or still-nonnested post-repair state
//! errors with the prior snapshot untouched (grants already applied stay
//! applied: Discord has no rollback). The 25-grant ceiling, additive-only
//! repair, live-identity fence, bot hierarchy, audit reason and shutdown
//! checks are unchanged.
//!
//! Remaining window. Revalidation narrows but cannot close the remote
//! check-to-use gap: Discord may change a member's roles between the final
//! read and its PUT, or between the last PUT and the post-repair refresh.
//! These tests pin the observable contract (no grant on proven-stale or
//! unproven evidence, publication from observed reads); they do not claim a
//! distributed atomic rank transaction.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex as StdMutex;

use serde_json::{json, Value};
use two_bot_core::activation::LiveCapability;
use two_bot_core::apply_web_contract;
use two_bot_testsupport::TestDatabase;

use super::*;
use crate::{
    activation::fixtures,
    discord_test_common::{MockRest, RestRequest, ScriptedResponse},
};

const GUILD: &str = "2222";
const BOT_ID: &str = "999";
const BOT_ROLES: &[&str] = &["10"];
const MEMBER_ONLY: &[&str] = &["12"];
const NESTED_PAIR: &[&str] = &["11", "12"];
const PROSPECT_ONLY: &[&str] = &["11"];

/// Ladder order, low to high: the granted rung must sit below a held rung.
fn rung_index(role: &str) -> Option<usize> {
    ["11", "12", "13", "14", "15"]
        .iter()
        .position(|rung| *rung == role)
}

/// One recorded grant plus whether the target still held a justifying higher
/// rung at the instant the PUT landed.
struct RecordedPut {
    member: String,
    role: String,
    eligible_at_put: bool,
}

/// Scripted external change applied once, right after the first PUT lands
/// (after the PUT sticks, so hooks observe post-grant truth).
enum FirstPutHook {
    /// Replace one member's truth outright.
    SetRoles { member: String, roles: Vec<String> },
    /// Clear every listed member except the one just granted: the mid-tick
    /// change hits whichever member the plan reaches second, regardless of
    /// plan order.
    ClearOthers { members: Vec<String> },
}

/// Scripted Discord truth behind the route-aware mock.
struct RankDiscord {
    /// The tick's roster capture, replayed frozen on every roster read.
    initial_roster: Value,
    /// Current per-member roles, observed on member reads.
    truth: HashMap<String, Vec<String>>,
    bot_roles: Vec<String>,
    roles_doc: Value,
    roster_calls: usize,
    puts: Vec<RecordedPut>,
    /// Forced status for single-member reads (departure, read failure).
    member_status: HashMap<String, u16>,
    /// Members whose read returns a record bound to a different user id.
    wrong_identity: HashSet<String>,
    /// Members whose read returns a record with no roles field.
    incomplete: HashSet<String>,
    /// Members whose read starts failing only after the first PUT lands
    /// (post-repair verification failure with healthy pre-grant evidence).
    fail_after_put: HashSet<String>,
    puts_stick: bool,
    on_first_put: Option<FirstPutHook>,
}

fn ladder_roles() -> Value {
    json!({"roles": [
        {"id":"10","name":"Bot","position":99},
        {"id":"11","name":"Prospect","position":1},
        {"id":"12","name":"Member","position":2},
        {"id":"13","name":"Soldier","position":3},
        {"id":"14","name":"Veteran","position":4},
        {"id":"15","name":"Legend","position":5},
    ]})
}

fn member(id: u64, bot: bool, roles: &[&str]) -> Value {
    json!({"user": {"id": id.to_string(), "bot": bot}, "roles": roles})
}

/// Build a mock universe: healthy control 1000 holding Prospect, the given
/// members in ascending id order, and three empty raid-window members. Truth
/// mirrors the capture until a test scripts a divergence.
fn discord_state(pairs: &[(u64, &[&str])]) -> RankDiscord {
    let mut roster = vec![member(1000, false, PROSPECT_ONLY)];
    let mut truth = HashMap::from([(
        "1000".to_owned(),
        PROSPECT_ONLY
            .iter()
            .map(|role| (*role).to_owned())
            .collect(),
    )]);
    for (id, roles) in pairs {
        roster.push(member(*id, false, roles));
        truth.insert(
            id.to_string(),
            roles.iter().map(|role| (*role).to_owned()).collect(),
        );
    }
    for raid in ["9000", "9001", "9002"] {
        roster.push(member(raid.parse().unwrap(), false, &[]));
        truth.insert(raid.to_owned(), Vec::new());
    }
    RankDiscord {
        initial_roster: Value::Array(roster),
        truth,
        bot_roles: BOT_ROLES.iter().map(|role| (*role).to_owned()).collect(),
        roles_doc: ladder_roles(),
        roster_calls: 0,
        puts: Vec::new(),
        member_status: HashMap::new(),
        wrong_identity: HashSet::new(),
        incomplete: HashSet::new(),
        fail_after_put: HashSet::new(),
        puts_stick: true,
        on_first_put: None,
    }
}

fn holds_higher(truth: &HashMap<String, Vec<String>>, member: &str, role: &str) -> bool {
    let Some(granted) = rung_index(role) else {
        return false;
    };
    truth.get(member).is_some_and(|held| {
        held.iter()
            .any(|have| rung_index(have).is_some_and(|index| index > granted))
    })
}

fn respond(state: &StdMutex<RankDiscord>, request: &RestRequest) -> ScriptedResponse {
    let mut state = state.lock().unwrap();
    let path = request
        .path
        .strip_prefix("/api/v10")
        .unwrap_or(request.path.as_str());
    let route = match path.split_once('?') {
        Some((route, _)) => route,
        None => path,
    };
    let guild_members = format!("/guilds/{GUILD}/members");
    let guild_route = format!("/guilds/{GUILD}");
    let member_prefix = format!("{guild_members}/");
    match request.method.as_str() {
        "GET" => {
            if route == guild_route.as_str() {
                return ScriptedResponse::json(200, state.roles_doc.clone());
            }
            if route == guild_members.as_str() {
                // Frozen capture: a roster replay is never revalidation.
                state.roster_calls += 1;
                return ScriptedResponse::json(200, state.initial_roster.clone());
            }
            if route == "/users/@me" {
                return ScriptedResponse::json(200, json!({"id": BOT_ID, "bot": true}));
            }
            if let Some(id) = route.strip_prefix(member_prefix.as_str()) {
                if let Some(status) = state.member_status.get(id) {
                    return ScriptedResponse::status(*status);
                }
                if !state.puts.is_empty() && state.fail_after_put.contains(id) {
                    return ScriptedResponse::status(500);
                }
                if id == BOT_ID {
                    return ScriptedResponse::json(
                        200,
                        json!({"user": {"id": BOT_ID, "bot": true}, "roles": state.bot_roles.clone()}),
                    );
                }
                if state.wrong_identity.contains(id) {
                    let other = if id == "1001" { "1002" } else { "1001" };
                    let roles = state.truth.get(id).cloned().unwrap_or_default();
                    return ScriptedResponse::json(
                        200,
                        json!({"user": {"id": other}, "roles": roles}),
                    );
                }
                if state.incomplete.contains(id) {
                    return ScriptedResponse::json(200, json!({"user": {"id": id}}));
                }
                let roles = state.truth.get(id).cloned().unwrap_or_default();
                return ScriptedResponse::json(
                    200,
                    json!({"user": {"id": id, "bot": false}, "roles": roles}),
                );
            }
            panic!("unexpected mock Discord route: GET {route}");
        }
        "PUT" => {
            let rest = route
                .strip_prefix(member_prefix.as_str())
                .unwrap_or_else(|| panic!("unexpected mock Discord route: PUT {route}"));
            let (id, role) = rest
                .split_once("/roles/")
                .unwrap_or_else(|| panic!("unexpected mock Discord route: PUT {route}"));
            let eligible_at_put = holds_higher(&state.truth, id, role);
            state.puts.push(RecordedPut {
                member: id.to_owned(),
                role: role.to_owned(),
                eligible_at_put,
            });
            if state.puts_stick {
                let held = state.truth.entry(id.to_owned()).or_default();
                if !held.iter().any(|have| have == role) {
                    held.push(role.to_owned());
                }
            }
            if state.puts.len() == 1 {
                match state.on_first_put.take() {
                    Some(FirstPutHook::SetRoles { member, roles }) => {
                        state.truth.insert(member, roles);
                    }
                    Some(FirstPutHook::ClearOthers { members }) => {
                        for other in members {
                            if other != id {
                                state.truth.insert(other, Vec::new());
                            }
                        }
                    }
                    None => {}
                }
            }
            ScriptedResponse::status(204)
        }
        method => panic!("unexpected mock Discord route: {method} {route}"),
    }
}

async fn discord(state: RankDiscord) -> (MockRest, Arc<StdMutex<RankDiscord>>) {
    let shared = Arc::new(StdMutex::new(state));
    let responder_state = Arc::clone(&shared);
    let mock =
        MockRest::with_responder(move |request: &RestRequest| respond(&responder_state, request))
            .await;
    (mock, shared)
}

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy(
        "synthetic-rank-repair-test-token".to_owned(),
        Some(mock.origin()),
    )
    .unwrap()
}

async fn tick(
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    observation: &Mutex<()>,
) -> Result<(), ErrorClass> {
    tick_gated(pool, rest, guild, observation, true).await
}

async fn tick_gated(
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    observation: &Mutex<()>,
    rank_heal: bool,
) -> Result<(), ErrorClass> {
    let (_stop, shutdown) = watch::channel(false);
    run_once(
        Kind::Rank,
        pool,
        rest,
        guild,
        observation,
        &shutdown,
        rank_heal,
    )
    .await
}

async fn tick_shutdown(
    pool: &PgPool,
    rest: &ActionExecutor,
    guild: &str,
    observation: &Mutex<()>,
) -> Result<(), ErrorClass> {
    let (_stop, shutdown) = watch::channel(true);
    run_once(Kind::Rank, pool, rest, guild, observation, &shutdown, true).await
}

/// Twilight percent-encodes `X-Audit-Log-Reason` on the wire, so assertions
/// decode the captured header first.
fn decode_reason(header: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let input = header.as_bytes();
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' {
            let hex = header.get(i + 1..i + 3)?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            bytes.push(input[i]);
            i += 1;
        }
    }
    String::from_utf8(bytes).ok()
}

/// Every PUT as `(member_id, role_id)` in arrival order.
fn puts(requests: &[RestRequest]) -> Vec<(String, String)> {
    let prefix = format!("/api/v10/guilds/{GUILD}/members/");
    requests
        .iter()
        .filter(|request| request.method == "PUT")
        .map(|request| {
            let rest = request.path.strip_prefix(prefix.as_str()).unwrap();
            let (member, role) = rest.split_once("/roles/").unwrap();
            (member.to_owned(), role.to_owned())
        })
        .collect()
}

fn assert_audit_reason(requests: &[RestRequest]) {
    for request in requests.iter().filter(|request| request.method == "PUT") {
        let reason = request.header("x-audit-log-reason").and_then(decode_reason);
        assert_eq!(
            reason.as_deref(),
            Some(RANK_SELF_HEAL_AUDIT_REASON),
            "repair grant carries the exact audit reason: {request:?}"
        );
    }
}

async fn db() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP rank repair acceptance: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated agent-testdb fixture"),
    )
}

async fn seed_raid_members(pool: &PgPool, guild: &str) {
    for (index, anomaly) in two_bot_core::RAID_ANOMALIES.iter().enumerate() {
        let (start, _) = two_bot_core::window_bounds(anomaly.start, anomaly.end).unwrap();
        sqlx::query("INSERT INTO members (guild_id, member_id, joined_at, is_bot) VALUES ($1,$2,$3::timestamptz,FALSE)")
            .bind(guild).bind((9000 + index).to_string()).bind(start).execute(pool).await.unwrap();
    }
}

async fn rank_of(pool: &PgPool, guild: &str, member: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT rank_key FROM member_ranks WHERE guild_id=$1 AND member_id=$2",
    )
    .bind(guild)
    .bind(member)
    .fetch_optional(pool)
    .await
    .unwrap()
    .flatten()
}

async fn holders(pool: &PgPool, guild: &str, key: &str) -> i32 {
    sqlx::query_scalar("SELECT holders_count FROM rank_snapshots WHERE guild_id=$1 AND rank_key=$2")
        .bind(guild)
        .bind(key)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn rank_table(pool: &PgPool, guild: &str) -> Vec<(String, i32, i32, String)> {
    sqlx::query_as::<_, (String, i32, i32, String)>(
        "SELECT rank_key, member_count, holders_count, snapshot_at FROM rank_snapshots WHERE guild_id=$1 ORDER BY rank_key",
    )
    .bind(guild)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn member_table(pool: &PgPool, guild: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT member_id, rank_key FROM member_ranks WHERE guild_id=$1 ORDER BY member_id",
    )
    .bind(guild)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn published_rows(pool: &PgPool, guild: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM rank_snapshots WHERE guild_id=$1")
        .bind(guild)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// RED: the higher rank is removed after the roster capture, so the member
/// read observes no holdings. No grant may land, and the observed-nested
/// roster publishes with the member unranked.
#[tokio::test]
async fn higher_rank_removed_before_grant_is_not_repaired() {
    let Some(fixture) = db().await else {
        return;
    };
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    seed_raid_members(&pool, GUILD).await;
    let mut state = discord_state(&[(1001, MEMBER_ONLY)]);
    state.truth.insert("1001".to_owned(), Vec::new());
    let (mock, shared) = discord(state).await;
    let observation = Arc::new(Mutex::new(()));
    let outcome = tick(&pool, &executor(&mock), GUILD, &observation).await;
    assert_eq!(outcome, Ok(()), "observed-nested roster publishes");
    let requests = mock.requests();
    assert!(
        puts(&requests).is_empty(),
        "removed higher rank must not authorize a grant: {requests:?}"
    );
    assert!(shared.lock().unwrap().puts.is_empty());
    assert_eq!(rank_of(&pool, GUILD, "1001").await, None);
    assert_eq!(
        rank_of(&pool, GUILD, "1000").await.as_deref(),
        Some("prospect")
    );
    assert_eq!(holders(&pool, GUILD, "prospect").await, 1);
    assert_eq!(holders(&pool, GUILD, "member").await, 0);
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}

/// RED: the affected member departs after the roster capture (member read is
/// 404). No grant may land, and the observed remainder publishes without the
/// departed member ranked.
#[tokio::test]
async fn departed_member_is_skipped_and_remainder_publishes() {
    let Some(fixture) = db().await else {
        return;
    };
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    seed_raid_members(&pool, GUILD).await;
    let mut state = discord_state(&[(1001, MEMBER_ONLY)]);
    state.member_status.insert("1001".to_owned(), 404);
    let (mock, shared) = discord(state).await;
    let observation = Arc::new(Mutex::new(()));
    let outcome = tick(&pool, &executor(&mock), GUILD, &observation).await;
    assert_eq!(outcome, Ok(()), "observed remainder publishes");
    let requests = mock.requests();
    assert!(
        puts(&requests).is_empty(),
        "departed member must not be granted: {requests:?}"
    );
    assert!(shared.lock().unwrap().puts.is_empty());
    assert_eq!(rank_of(&pool, GUILD, "1001").await, None);
    assert_eq!(
        rank_of(&pool, GUILD, "1000").await.as_deref(),
        Some("prospect")
    );
    assert_eq!(holders(&pool, GUILD, "prospect").await, 1);
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}

/// RED: the member re-read fails, so eligibility is unproven. The whole tick
/// fails closed with no PUT and no publication: a failed read must not read
/// as an empty membership.
#[tokio::test]
async fn failed_member_reread_fails_the_tick_closed() {
    let Some(fixture) = db().await else {
        return;
    };
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    seed_raid_members(&pool, GUILD).await;
    let mut state = discord_state(&[(1001, MEMBER_ONLY)]);
    state.member_status.insert("1001".to_owned(), 500);
    let (mock, _shared) = discord(state).await;
    let observation = Arc::new(Mutex::new(()));
    let outcome = tick(&pool, &executor(&mock), GUILD, &observation).await;
    assert!(
        outcome.is_err(),
        "unverifiable member evidence fails closed"
    );
    let requests = mock.requests();
    assert!(
        puts(&requests).is_empty(),
        "failed revalidation must not grant: {requests:?}"
    );
    assert_eq!(
        published_rows(&pool, GUILD).await,
        0,
        "failed revalidation publishes nothing"
    );
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}

/// RED: the member re-read returns a record bound to a different user id.
/// Unproven evidence fails the whole tick closed with no PUT and no
/// publication.
#[tokio::test]
async fn wrong_identity_member_evidence_fails_the_tick_closed() {
    let Some(fixture) = db().await else {
        return;
    };
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    seed_raid_members(&pool, GUILD).await;
    let mut state = discord_state(&[(1001, MEMBER_ONLY)]);
    state.wrong_identity.insert("1001".to_owned());
    let (mock, _shared) = discord(state).await;
    let observation = Arc::new(Mutex::new(()));
    let outcome = tick(&pool, &executor(&mock), GUILD, &observation).await;
    assert!(outcome.is_err(), "misbound member evidence fails closed");
    let requests = mock.requests();
    assert!(
        puts(&requests).is_empty(),
        "misbound evidence must not grant: {requests:?}"
    );
    assert_eq!(
        published_rows(&pool, GUILD).await,
        0,
        "misbound evidence publishes nothing"
    );
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}

/// RED: the member re-read returns a record with no roles field. Unproven
/// evidence fails the whole tick closed with no PUT and no publication.
#[tokio::test]
async fn incomplete_member_evidence_fails_the_tick_closed() {
    let Some(fixture) = db().await else {
        return;
    };
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    seed_raid_members(&pool, GUILD).await;
    let mut state = discord_state(&[(1001, MEMBER_ONLY)]);
    state.incomplete.insert("1001".to_owned());
    let (mock, _shared) = discord(state).await;
    let observation = Arc::new(Mutex::new(()));
    let outcome = tick(&pool, &executor(&mock), GUILD, &observation).await;
    assert!(outcome.is_err(), "incomplete member evidence fails closed");
    let requests = mock.requests();
    assert!(
        puts(&requests).is_empty(),
        "incomplete evidence must not grant: {requests:?}"
    );
    assert_eq!(
        published_rows(&pool, GUILD).await,
        0,
        "incomplete evidence publishes nothing"
    );
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}

/// RED: the member the plan reaches second loses its higher rank after the
/// first PUT lands. Only the still-eligible grant lands, every grant held
/// its higher rank at PUT time, and publication matches observed truth.
#[tokio::test]
async fn role_removed_between_grants_skips_the_second_grant() {
    let Some(fixture) = db().await else {
        return;
    };
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    seed_raid_members(&pool, GUILD).await;
    let mut state = discord_state(&[(1001, MEMBER_ONLY), (1002, MEMBER_ONLY)]);
    state.on_first_put = Some(FirstPutHook::ClearOthers {
        members: ["1001".to_owned(), "1002".to_owned()].into(),
    });
    let (mock, shared) = discord(state).await;
    let observation = Arc::new(Mutex::new(()));
    let outcome = tick(&pool, &executor(&mock), GUILD, &observation).await;
    assert_eq!(outcome, Ok(()), "observed repair publishes");
    let requests = mock.requests();
    assert_audit_reason(&requests);
    let grants = puts(&requests);
    assert_eq!(
        grants.len(),
        1,
        "only the still-eligible member is repaired: {grants:?}"
    );
    {
        let seen = shared.lock().unwrap();
        assert_eq!(
            seen.puts
                .iter()
                .map(|put| (put.member.clone(), put.role.clone()))
                .collect::<Vec<_>>(),
            grants,
            "mock grant log matches the wire"
        );
        assert!(
            seen.puts.iter().all(|put| put.eligible_at_put),
            "every grant held its higher rank at PUT time"
        );
    }
    let (granted, rung) = &grants[0];
    assert_eq!(rung, "11");
    assert_eq!(
        rank_of(&pool, GUILD, granted).await.as_deref(),
        Some("member")
    );
    let other = if granted == "1001" { "1002" } else { "1001" };
    assert_eq!(rank_of(&pool, GUILD, other).await, None);
    assert_eq!(holders(&pool, GUILD, "prospect").await, 2);
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}

/// RED: a higher rank granted out of band after the repair PUT is part of
/// the observed post-repair roles, so publication reports it instead of the
/// optimistic lower rank.
#[tokio::test]
async fn externally_added_rank_between_grant_and_publish_is_observed() {
    let Some(fixture) = db().await else {
        return;
    };
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    seed_raid_members(&pool, GUILD).await;
    let mut state = discord_state(&[(1001, MEMBER_ONLY)]);
    state.on_first_put = Some(FirstPutHook::SetRoles {
        member: "1001".to_owned(),
        roles: ["11".to_owned(), "12".to_owned(), "13".to_owned()].into(),
    });
    let (mock, _shared) = discord(state).await;
    let observation = Arc::new(Mutex::new(()));
    let outcome = tick(&pool, &executor(&mock), GUILD, &observation).await;
    assert_eq!(outcome, Ok(()), "observed repair publishes");
    let requests = mock.requests();
    assert_audit_reason(&requests);
    assert_eq!(puts(&requests), [("1001".to_owned(), "11".to_owned())]);
    assert_eq!(
        rank_of(&pool, GUILD, "1001").await.as_deref(),
        Some("soldier"),
        "publication reports observed post-repair roles"
    );
    assert_eq!(holders(&pool, GUILD, "soldier").await, 1);
    assert_eq!(holders(&pool, GUILD, "member").await, 1);
    assert_eq!(holders(&pool, GUILD, "prospect").await, 2);
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}

/// RED: the grant lands but the post-repair member read fails, so the tick
/// errors and the prior published snapshot is byte-identical (snapshot stamp
/// included: no optimistic rewrite).
#[tokio::test]
async fn failed_post_repair_verification_keeps_prior_snapshot() {
    let Some(fixture) = db().await else {
        return;
    };
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    seed_raid_members(&pool, GUILD).await;
    let observation = Arc::new(Mutex::new(()));
    let (baseline, _) = discord(discord_state(&[(1001, PROSPECT_ONLY)])).await;
    tick(&pool, &executor(&baseline), GUILD, &observation)
        .await
        .expect("nested baseline publishes");
    baseline.shutdown().await;
    let before_ranks = rank_table(&pool, GUILD).await;
    let before_members = member_table(&pool, GUILD).await;
    assert!(!before_ranks.is_empty(), "baseline published");
    let mut state = discord_state(&[(1001, MEMBER_ONLY)]);
    state.fail_after_put.insert("1001".to_owned());
    let (mock, _shared) = discord(state).await;
    let outcome = tick(&pool, &executor(&mock), GUILD, &observation).await;
    assert!(
        outcome.is_err(),
        "unverifiable post-repair state errors honestly"
    );
    let requests = mock.requests();
    let grants = puts(&requests);
    assert!(
        !grants.is_empty(),
        "the grant was applied before verification failed: {requests:?}"
    );
    assert!(
        grants
            .iter()
            .all(|(member, role)| member == "1001" && role == "11"),
        "only the planned grant was attempted: {grants:?}"
    );
    assert_eq!(
        rank_table(&pool, GUILD).await,
        before_ranks,
        "prior rank snapshot unchanged"
    );
    assert_eq!(
        member_table(&pool, GUILD).await,
        before_members,
        "prior member ranks unchanged"
    );
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}

/// RED: the grant PUT is accepted but the post-repair member read still
/// shows the missing rung, so the tick errors and the prior published
/// snapshot is byte-identical.
#[tokio::test]
async fn non_sticking_grant_keeps_prior_snapshot() {
    let Some(fixture) = db().await else {
        return;
    };
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    seed_raid_members(&pool, GUILD).await;
    let observation = Arc::new(Mutex::new(()));
    let (baseline, _) = discord(discord_state(&[(1001, PROSPECT_ONLY)])).await;
    tick(&pool, &executor(&baseline), GUILD, &observation)
        .await
        .expect("nested baseline publishes");
    baseline.shutdown().await;
    let before_ranks = rank_table(&pool, GUILD).await;
    let before_members = member_table(&pool, GUILD).await;
    assert!(!before_ranks.is_empty(), "baseline published");
    let mut state = discord_state(&[(1001, MEMBER_ONLY)]);
    state.puts_stick = false;
    let (mock, _shared) = discord(state).await;
    let outcome = tick(&pool, &executor(&mock), GUILD, &observation).await;
    assert!(
        outcome.is_err(),
        "still-nonnested post-repair state errors honestly"
    );
    let requests = mock.requests();
    let grants = puts(&requests);
    assert!(
        !grants.is_empty(),
        "the grant was attempted before verification: {requests:?}"
    );
    assert!(
        grants
            .iter()
            .all(|(member, role)| member == "1001" && role == "11"),
        "only the planned grant was attempted: {grants:?}"
    );
    assert_eq!(
        rank_table(&pool, GUILD).await,
        before_ranks,
        "prior rank snapshot unchanged"
    );
    assert_eq!(
        member_table(&pool, GUILD).await,
        before_members,
        "prior member ranks unchanged"
    );
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}

/// GREEN control: the ordinary repair path is unchanged. Member 1001 holds
/// Member without Prospect, the tick grants exactly the missing lower rung
/// with the exact audit reason, and publishes the repaired ladder.
#[tokio::test]
async fn valid_repair_still_heals_and_publishes() {
    let Some(fixture) = db().await else {
        return;
    };
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    seed_raid_members(&pool, GUILD).await;
    let (mock, shared) = discord(discord_state(&[(1001, MEMBER_ONLY)])).await;
    let observation = Arc::new(Mutex::new(()));
    let outcome = tick(&pool, &executor(&mock), GUILD, &observation).await;
    assert_eq!(outcome, Ok(()), "valid repair publishes");
    let requests = mock.requests();
    assert_audit_reason(&requests);
    assert_eq!(puts(&requests), [("1001".to_owned(), "11".to_owned())]);
    assert!(
        shared.lock().unwrap().roster_calls >= 1,
        "roster was captured"
    );
    assert_eq!(
        rank_of(&pool, GUILD, "1001").await.as_deref(),
        Some("member")
    );
    assert_eq!(holders(&pool, GUILD, "prospect").await, 2);
    assert_eq!(holders(&pool, GUILD, "member").await, 1);
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}

/// GREEN control: an already-nested roster publishes with no PUT.
#[tokio::test]
async fn already_nested_roster_publishes_without_puts() {
    let Some(fixture) = db().await else {
        return;
    };
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    seed_raid_members(&pool, GUILD).await;
    let (mock, _shared) = discord(discord_state(&[(1001, NESTED_PAIR)])).await;
    let observation = Arc::new(Mutex::new(()));
    let outcome = tick(&pool, &executor(&mock), GUILD, &observation).await;
    assert_eq!(outcome, Ok(()), "nested roster publishes");
    let requests = mock.requests();
    assert!(
        puts(&requests).is_empty(),
        "nested roster sends no grant: {requests:?}"
    );
    assert_eq!(
        rank_of(&pool, GUILD, "1001").await.as_deref(),
        Some("member")
    );
    assert_eq!(holders(&pool, GUILD, "prospect").await, 2);
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}

/// GREEN control: 26 planned grants keep the existing 25-grant ceiling
/// refusal with no PUT and no publication.
#[tokio::test]
async fn over_cap_repair_still_fails_closed() {
    let Some(fixture) = db().await else {
        return;
    };
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    seed_raid_members(&pool, GUILD).await;
    let pairs: Vec<(u64, &[&str])> = (1001..=1026).map(|id| (id, MEMBER_ONLY)).collect();
    assert!(pairs.len() > MAX_RANK_SELF_HEAL_GRANTS_PER_TICK);
    let (mock, _shared) = discord(discord_state(&pairs)).await;
    let observation = Arc::new(Mutex::new(()));
    let outcome = tick(&pool, &executor(&mock), GUILD, &observation).await;
    assert_eq!(outcome, Err(ErrorClass::Configuration));
    let requests = mock.requests();
    assert!(
        puts(&requests).is_empty(),
        "over-cap repair must not mutate: {requests:?}"
    );
    assert_eq!(published_rows(&pool, GUILD).await, 0);
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}

/// GREEN control: a refused identity never grants. The staging pair is
/// cleared for the rank-heal capability; every refused fixture keeps the
/// `Configuration` refusal with no PUT and nothing published.
#[tokio::test]
async fn refused_identity_still_sends_no_put() {
    assert!(
        fixtures::staging().permitted(LiveCapability::RankHeal),
        "staging pair heals rank ladders"
    );
    for (label, activation) in fixtures::refused() {
        assert!(
            !activation.permitted(LiveCapability::RankHeal),
            "{label} must not heal rank ladders"
        );
    }
    let Some(fixture) = db().await else {
        return;
    };
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    seed_raid_members(&pool, GUILD).await;
    let (mock, _shared) = discord(discord_state(&[(1001, MEMBER_ONLY)])).await;
    let observation = Arc::new(Mutex::new(()));
    let outcome = tick_gated(&pool, &executor(&mock), GUILD, &observation, false).await;
    assert_eq!(outcome, Err(ErrorClass::Configuration));
    let requests = mock.requests();
    assert!(
        puts(&requests).is_empty(),
        "refused heal must not mutate: {requests:?}"
    );
    assert_eq!(published_rows(&pool, GUILD).await, 0);
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}

/// GREEN control: a ladder missing the top rung refuses with
/// `Configuration`, writes nothing, and sends no Discord write.
#[tokio::test]
async fn invalid_ladder_still_fails_closed() {
    let Some(fixture) = db().await else {
        return;
    };
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    seed_raid_members(&pool, GUILD).await;
    let mut state = discord_state(&[(1001, MEMBER_ONLY)]);
    state.roles_doc = json!({"roles": [
        {"id":"10","name":"Bot","position":99},
        {"id":"11","name":"Prospect","position":1},
        {"id":"12","name":"Member","position":2},
        {"id":"13","name":"Soldier","position":3},
        {"id":"14","name":"Veteran","position":4},
    ]});
    let (mock, _shared) = discord(state).await;
    let observation = Arc::new(Mutex::new(()));
    let outcome = tick(&pool, &executor(&mock), GUILD, &observation).await;
    assert_eq!(outcome, Err(ErrorClass::Configuration));
    let requests = mock.requests();
    assert!(
        puts(&requests).is_empty(),
        "invalid ladder must not mutate: {requests:?}"
    );
    for request in &requests {
        assert_eq!(request.method, "GET", "refused tick sends no Discord write");
    }
    assert_eq!(published_rows(&pool, GUILD).await, 0);
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}

/// GREEN control: a tick that starts stopped performs no observation at all.
#[tokio::test]
async fn shutdown_tick_sends_no_requests() {
    let Some(fixture) = db().await else {
        return;
    };
    let pool = fixture.pool().clone();
    apply_web_contract(&pool).await.unwrap();
    seed_raid_members(&pool, GUILD).await;
    let (mock, _shared) = discord(discord_state(&[(1001, MEMBER_ONLY)])).await;
    let observation = Arc::new(Mutex::new(()));
    let outcome = tick_shutdown(&pool, &executor(&mock), GUILD, &observation).await;
    assert_eq!(outcome, Ok(()));
    assert!(mock.requests().is_empty(), "stopped tick sends no requests");
    assert_eq!(published_rows(&pool, GUILD).await, 0);
    mock.shutdown().await;
    fixture
        .close()
        .await
        .expect("drop disposable test database");
}
