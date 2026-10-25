//! Acceptance coverage for nonexclusive button and reaction panels.
//!
//! Contract under test: on a nonexclusive panel an add, an explicit remove or a
//! no-op concerns exactly one audited option. Execution, compensation,
//! interrupted-exchange recovery and lease-expiry replay converge that option
//! and nothing else, and settlement checks that option against current
//! authoritative evidence. Another offered option that changes after the event
//! was prepared is never added, deleted or restored on its behalf, and its
//! change is never credited to this event. Select-menu submissions stay
//! exact-set, exclusive panels keep their replacement and lane rules, and the
//! claim, receipt and live-validation fences stay as they are.
//!
//! The fixtures use only agent-testdb with a generated schema and a stateful
//! loopback Discord double: no credentials, guilds, staging or production
//! resources. Tests named `*_unchanged` are controls that hold before and after
//! the behavior change; every other test fails until the contract is met.

#![cfg(test)]

#[path = "self_role_refusal_acceptance_tests.rs"]
mod refusal_acceptance;

use super::*;
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::self_role_handlers::SelfRoleService;
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    sync::{
        atomic::{AtomicI64, AtomicUsize},
        Mutex,
    },
};
use two_bot_core::self_roles::{event_order_for_event_id, SelfRoleGates, SelfRoleOption};

const GUILD: &str = "100000000000000001";
const USER: &str = "100000000000000002";
const BOT: &str = "100000000000000003";
/// Option "bravo": the unrelated offered option whose state changes under A.
const ROLE_B: &str = "100000000000000004";
const BOT_ROLE: &str = "100000000000000005";
/// A role the member holds that no panel offers; it is never touched.
const UNRELATED: &str = "100000000000000006";
const CHANNEL: &str = "100000000000000007";
const MESSAGE: &str = "100000000000000008";
/// Option "alpha": the audited option of the event under test.
const ROLE_A: &str = "100000000000000009";
const NOW: i64 = 1_700_000_000_000;
const RENDEZVOUS: Duration = Duration::from_secs(30);

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const MIGRATIONS: [&str; 7] = [
    include_str!("../../cutover/migrations/0200_self_roles.sql"),
    include_str!("../../cutover/migrations/0201_self_role_intent_initialization.sql"),
    include_str!("../../cutover/migrations/0202_self_role_compensation_phase.sql"),
    include_str!("../../cutover/migrations/0203_self_role_pending_exchange.sql"),
    include_str!("../../cutover/migrations/0204_self_role_terminal_repair.sql"),
    include_str!("../../cutover/migrations/0205_self_role_exchange_receipts.sql"),
    include_str!("../../cutover/migrations/0206_self_role_exchange_baselines.sql"),
];

// ---------------------------------------------------------------------------
// Isolated database
// ---------------------------------------------------------------------------

/// One generated schema per scenario, dropped even when the scenario panics.
async fn isolated<F, Fut>(scenario: F) -> TestResult
where
    F: FnOnce(PgPool) -> Fut,
    Fut: Future<Output = TestResult> + Send + 'static,
{
    let options = PgConnectOptions::new()
        .host("agent-testdb")
        .port(5432)
        .username("agent_test")
        .password("")
        .database("agent_test");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    // Fixed prefix plus numeric PID/timestamp; no caller-supplied identifiers.
    let schema = format!("self_role_nonexclusive_{}_{}", std::process::id(), nonce);
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await?;
    let search_path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .after_connect(move |conn, _| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path', $1, false)")
                    .bind(search_path)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await?;
    for migration in MIGRATIONS {
        sqlx::raw_sql(migration).execute(&pool).await?;
    }
    let outcome = tokio::spawn(scenario(pool.clone())).await;
    pool.close().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await?;
    admin.close().await;
    match outcome {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(Box::new(error)),
    }
}

// ---------------------------------------------------------------------------
// Stateful Discord double
// ---------------------------------------------------------------------------

/// Authoritative member and policy state. Every member read reflects the
/// current role set, so a scenario can change an unrelated option at any point.
#[derive(Default)]
struct GuildState {
    held: BTreeSet<String>,
    above_bot: BTreeSet<String>,
    faults: HashMap<String, VecDeque<Fault>>,
}

/// A scripted outcome for the next mutation of one role. `applied` says whether
/// Discord performed the change; `concurrent` lists unrelated changes another
/// actor makes at the same moment.
#[derive(Clone)]
struct Fault {
    status: u16,
    applied: bool,
    concurrent: Vec<Change>,
}

#[derive(Clone)]
enum Change {
    Grant(&'static str),
    Revoke(&'static str),
}

#[derive(Clone)]
struct Guild(Arc<Mutex<GuildState>>);

impl Guild {
    fn new(held: &[&str]) -> Self {
        let state = GuildState {
            held: held.iter().map(|role| (*role).to_owned()).collect(),
            ..GuildState::default()
        };
        Self(Arc::new(Mutex::new(state)))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, GuildState> {
        self.0.lock().unwrap()
    }

    /// An independent change made by someone else (another event, a moderator).
    fn grant(&self, role: &str) {
        self.state().held.insert(role.to_owned());
    }

    fn revoke(&self, role: &str) {
        self.state().held.remove(role);
    }

    /// Move a role above the bot's highest role in the live hierarchy.
    fn raise_above_bot(&self, role: &str) {
        self.state().above_bot.insert(role.to_owned());
    }

    fn fail_next(&self, method: &str, role: &str, fault: Fault) {
        self.state()
            .faults
            .entry(format!("{method} {role}"))
            .or_default()
            .push_back(fault);
    }

    fn held_labels(&self) -> BTreeSet<String> {
        self.state()
            .held
            .iter()
            .map(|role| label(role).to_owned())
            .collect()
    }

    async fn serve(&self) -> MockRest {
        let state = Arc::clone(&self.0);
        MockRest::with_responder(move |request: &RestRequest| respond(&state, request)).await
    }
}

fn respond(state: &Mutex<GuildState>, request: &RestRequest) -> ScriptedResponse {
    let Some(path) = request.path.strip_prefix("/api/v10") else {
        return ScriptedResponse::status(404);
    };
    let mut guild = state.lock().unwrap();
    let member = format!("/guilds/{GUILD}/members/{USER}");
    let bot = format!("/guilds/{GUILD}/members/{BOT}");
    let roles = format!("/guilds/{GUILD}/roles");
    let channels = format!("/guilds/{GUILD}/channels");
    let message = format!("/channels/{CHANNEL}/messages/{MESSAGE}");
    let mutation = format!("{member}/roles/");
    match (request.method.as_str(), path) {
        ("GET", p) if p == member => {
            ScriptedResponse::json(200, json!({"user":{"id":USER},"roles":guild.held}))
        }
        ("GET", p) if p == bot => ScriptedResponse::json(
            200,
            json!({"user":{"id":BOT,"bot":true},"roles":[BOT_ROLE]}),
        ),
        ("GET", p) if p == roles => {
            let position = |role: &str, base: i64| {
                if guild.above_bot.contains(role) {
                    11
                } else {
                    base
                }
            };
            ScriptedResponse::json(
                200,
                json!([
                    {"id":GUILD,"permissions":"1024","position":0,"managed":false,"color":0},
                    {"id":ROLE_B,"permissions":"0","position":position(ROLE_B, 2),"managed":false,"color":0},
                    {"id":ROLE_A,"permissions":"0","position":position(ROLE_A, 3),"managed":false,"color":0},
                    {"id":BOT_ROLE,"permissions":"268435456","position":10,"managed":true,"color":0},
                    {"id":UNRELATED,"permissions":"0","position":1,"managed":false,"color":0}
                ]),
            )
        }
        ("GET", p) if p == channels => ScriptedResponse::json(
            200,
            json!([{"id":CHANNEL,"guild_id":GUILD,"name":"games","permission_overwrites":[]}]),
        ),
        ("GET", p) if p == message => {
            ScriptedResponse::json(200, json!({"id":MESSAGE,"channel_id":CHANNEL}))
        }
        ("PUT" | "DELETE", p) if p.starts_with(&mutation) => {
            let role = &p[mutation.len()..];
            let adding = request.method == "PUT";
            let key = format!("{} {role}", request.method);
            let fault = guild.faults.get_mut(&key).and_then(VecDeque::pop_front);
            let (status, applied, concurrent) = match fault {
                Some(fault) => (fault.status, fault.applied, fault.concurrent),
                None => (204, true, Vec::new()),
            };
            if applied {
                if adding {
                    guild.held.insert(role.to_owned());
                } else {
                    guild.held.remove(role);
                }
            }
            for change in concurrent {
                match change {
                    Change::Grant(other) => {
                        guild.held.insert(other.to_owned());
                    }
                    Change::Revoke(other) => {
                        guild.held.remove(other);
                    }
                }
            }
            if status == 204 {
                ScriptedResponse::status(204)
            } else {
                ScriptedResponse::json(status, json!({"message":"fixture refusal"}))
            }
        }
        _ => ScriptedResponse::status(404),
    }
}

fn label(role: &str) -> &str {
    match role {
        ROLE_A => "A",
        ROLE_B => "B",
        UNRELATED => "UNRELATED",
        other => other,
    }
}

fn put(role: &str) -> String {
    format!("PUT {}", label(role))
}

fn delete(role: &str) -> String {
    format!("DELETE {}", label(role))
}

fn ids(roles: &[&str]) -> Vec<String> {
    roles.iter().map(|role| (*role).to_owned()).collect()
}

// ---------------------------------------------------------------------------
// Runtime fixture
// ---------------------------------------------------------------------------

fn runtime(pool: &PgPool, clock: &Arc<AtomicI64>, mock: &MockRest) -> SelfRoleRuntime {
    SelfRoleRuntime {
        store: SelfRoleStore::with_test_clock(pool.clone(), 300, clock.clone()).unwrap(),
        executor: ActionExecutor::with_proxy_and_moderation_timeout(
            "fixture-token".into(),
            Some(mock.origin()),
            RENDEZVOUS,
        )
        .unwrap(),
        guild_id: GUILD.into(),
        bot_id: BOT.into(),
    }
}

struct Fixture {
    pool: PgPool,
    clock: Arc<AtomicI64>,
    guild: Guild,
    mock: MockRest,
    runtime: SelfRoleRuntime,
    seen: AtomicUsize,
}

impl Fixture {
    async fn new(pool: &PgPool, held: &[&str]) -> Self {
        let guild = Guild::new(held);
        let mock = guild.serve().await;
        let clock = Arc::new(AtomicI64::new(NOW));
        let runtime = runtime(pool, &clock, &mock);
        Self {
            pool: pool.clone(),
            clock,
            guild,
            mock,
            runtime,
            seen: AtomicUsize::new(0),
        }
    }

    /// A second runtime on the same store, clock and Discord double: a process
    /// restart that keeps only durable state.
    fn restarted(&self) -> SelfRoleRuntime {
        runtime(&self.pool, &self.clock, &self.mock)
    }

    fn advance(&self, ms: i64) {
        self.clock.store(NOW + ms, Ordering::SeqCst);
    }

    /// Every role mutation the bot sent, in arrival order, as `PUT A`,
    /// `DELETE B` and so on. Anything off the singular role route shows up
    /// verbatim, so a bulk edit cannot pass as a precise one.
    fn all_mutations(&self) -> Vec<String> {
        let prefix = format!("/api/v10/guilds/{GUILD}/members/{USER}/roles/");
        self.mock
            .requests()
            .into_iter()
            .filter(|request| request.method != "GET")
            .map(|request| {
                assert!(request.body.is_empty(), "role mutations carry no body");
                match request.path.strip_prefix(&prefix) {
                    Some(role) => format!("{} {}", request.method, label(role)),
                    None => format!("{} {}", request.method, request.path),
                }
            })
            .collect()
    }

    /// Mutations sent since the previous call.
    fn new_mutations(&self) -> Vec<String> {
        let all = self.all_mutations();
        let from = self.seen.swap(all.len(), Ordering::SeqCst);
        all[from..].to_vec()
    }

    fn assert_held(&self, expected: &[&str]) {
        let expected: BTreeSet<String> =
            expected.iter().map(|role| label(role).to_owned()).collect();
        assert_eq!(
            self.guild.held_labels(),
            expected,
            "the member's final roles (A and B are the offered options)"
        );
    }
}

fn catalogue(mode: PanelMode, exclusive: bool, id: &str) -> SelfRolePanel {
    SelfRolePanel {
        id: id.into(),
        channel_id: CHANNEL.into(),
        message_id: MESSAGE.into(),
        mode,
        exclusive,
        color: false,
        options: vec![option("alpha", ROLE_A, "a"), option("bravo", ROLE_B, "b")],
    }
}

fn option(key: &str, role: &str, emoji: &str) -> SelfRoleOption {
    SelfRoleOption {
        key: key.into(),
        label: key.into(),
        role_id: role.into(),
        permissions: "0".into(),
        emoji: Some(emoji.into()),
        description: None,
    }
}

fn request(id: &str, selection: Selection) -> SelfRoleRequest {
    SelfRoleRequest {
        event_id: id.into(),
        event_order: event_order_for_event_id(id, NOW as u64),
        guild_id: GUILD.into(),
        member_id: USER.into(),
        channel_id: CHANNEL.into(),
        message_id: MESSAGE.into(),
        selection,
    }
}

fn click(id: &str, key: &str) -> SelfRoleRequest {
    request(
        id,
        Selection::Button {
            option_key: key.into(),
        },
    )
}

fn react(id: &str, key: &str, remove: bool) -> SelfRoleRequest {
    request(
        id,
        Selection::Reaction {
            option_key: key.into(),
            remove,
        },
    )
}

fn choose(id: &str, keys: &[&str]) -> SelfRoleRequest {
    request(
        id,
        Selection::Select {
            option_keys: keys.iter().map(|key| (*key).to_owned()).collect(),
        },
    )
}

// ---------------------------------------------------------------------------
// Flow helpers
// ---------------------------------------------------------------------------

fn ok<T>(result: Result<T, RuntimeError>) -> TestResult<T> {
    result.map_err(|error| format!("runtime error: {error:?}").into())
}

async fn admit(
    runtime: &SelfRoleRuntime,
    request: &SelfRoleRequest,
    panel: &SelfRolePanel,
) -> TestResult<Box<PreparedSelfRole>> {
    match ok(runtime.prepare(request, panel).await)? {
        Admission::Ready(prepared) => Ok(prepared),
        Admission::Rejected(code) => Err(format!("admission rejected: {code}").into()),
        Admission::Duplicate => Err("admission reported a duplicate".into()),
        Admission::Ignored => Err("admission ignored the event".into()),
    }
}

/// Durable discovery plus a fresh claim, as the recovery sweep does it.
async fn readmit(
    runtime: &SelfRoleRuntime,
    panel: &SelfRolePanel,
) -> TestResult<Box<PreparedSelfRole>> {
    let candidates = ok(runtime.recovery_candidates(panel, 8).await)?;
    assert_eq!(candidates.len(), 1, "one expired processing event");
    match ok(runtime.recover(&candidates[0], panel).await)? {
        Admission::Ready(prepared) => Ok(prepared),
        _ => Err("recovery did not hand the event back".into()),
    }
}

async fn finish(
    runtime: &SelfRoleRuntime,
    prepared: &mut PreparedSelfRole,
    panel: &SelfRolePanel,
) -> TestResult<SettledOutcome> {
    let execution = ok(runtime.execute(prepared, panel).await)?;
    ok(runtime.settle(prepared, panel, execution).await)
}

/// An independent event run start to finish.
async fn complete(
    runtime: &SelfRoleRuntime,
    request: &SelfRoleRequest,
    panel: &SelfRolePanel,
) -> TestResult<SettledOutcome> {
    let mut prepared = admit(runtime, request, panel).await?;
    finish(runtime, &mut prepared, panel).await
}

fn recovery_service(
    runtime: SelfRoleRuntime,
    panel: &SelfRolePanel,
) -> TestResult<SelfRoleService> {
    let allowlist = [GUILD.to_owned()].into_iter().collect();
    let gates = SelfRoleGates {
        panels: vec![panel.clone()],
        dry_run: false,
    };
    SelfRoleService::new(runtime, gates, &allowlist)
        .ok_or_else(|| "recovery service refused the fixture catalogue".into())
}

// ---------------------------------------------------------------------------
// Stored evidence
// ---------------------------------------------------------------------------

struct Stored {
    outcome: String,
    code: Option<String>,
    pending: bool,
    effects: AuditEffects,
    desired: Vec<String>,
    before: Vec<String>,
}

type AuditColumns = (
    String,
    Option<String>,
    bool,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);

async fn stored(pool: &PgPool, event: &str) -> TestResult<Stored> {
    let row: AuditColumns = sqlx::query_as(
        "SELECT outcome,code,exchange_pending,added_role_ids,removed_role_ids,
         attempted_added_role_ids,attempted_removed_role_ids,compensated_added_role_ids,
         compensated_removed_role_ids,unresolved_added_role_ids,unresolved_removed_role_ids,
         desired_role_ids,pre_mutation_role_ids FROM self_role_audit WHERE event_id=$1",
    )
    .bind(event)
    .fetch_one(pool)
    .await?;
    let list = |text: &str| -> TestResult<Vec<String>> { Ok(serde_json::from_str(text)?) };
    Ok(Stored {
        outcome: row.0,
        code: row.1,
        pending: row.2,
        effects: AuditEffects {
            added_role_ids: list(&row.3)?,
            removed_role_ids: list(&row.4)?,
            attempted_added_role_ids: list(&row.5)?,
            attempted_removed_role_ids: list(&row.6)?,
            compensated_added_role_ids: list(&row.7)?,
            compensated_removed_role_ids: list(&row.8)?,
            unresolved_added_role_ids: list(&row.9)?,
            unresolved_removed_role_ids: list(&row.10)?,
        },
        desired: list(&row.11)?,
        before: list(&row.12)?,
    })
}

/// Role ids of the durable exchange receipts an event journaled.
async fn receipt_roles(pool: &PgPool, event: &str) -> TestResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT role_id FROM self_role_exchanges WHERE event_id=$1 ORDER BY created_at,exchange_id",
    )
    .bind(event)
    .fetch_all(pool)
    .await?)
}

async fn lane_target(pool: &PgPool, panel: &str) -> TestResult<(Option<String>, bool)> {
    Ok(sqlx::query_as(
        "SELECT latest_option_key,target_committed FROM self_role_panel_claims
         WHERE guild_id=$1 AND member_id=$2 AND panel_id=$3",
    )
    .bind(GUILD)
    .bind(USER)
    .bind(panel)
    .fetch_one(pool)
    .await?)
}

/// Evidence of exactly one added role, as the audit records a clean add.
fn clean_add(role: &str) -> AuditEffects {
    AuditEffects {
        added_role_ids: ids(&[role]),
        attempted_added_role_ids: ids(&[role]),
        ..AuditEffects::default()
    }
}

fn clean_remove(role: &str) -> AuditEffects {
    AuditEffects {
        removed_role_ids: ids(&[role]),
        attempted_removed_role_ids: ids(&[role]),
        ..AuditEffects::default()
    }
}

fn mentions(effects: &AuditEffects, role: &str) -> bool {
    [
        &effects.added_role_ids,
        &effects.removed_role_ids,
        &effects.attempted_added_role_ids,
        &effects.attempted_removed_role_ids,
        &effects.compensated_added_role_ids,
        &effects.compensated_removed_role_ids,
        &effects.unresolved_added_role_ids,
        &effects.unresolved_removed_role_ids,
    ]
    .iter()
    .any(|ids| ids.iter().any(|id| id == role))
}

/// The settled audit must carry this outcome and exactly these effects: the
/// attribution of one event's changes to that event alone.
async fn assert_audit(
    pool: &PgPool,
    event: &str,
    outcome: &str,
    effects: AuditEffects,
) -> TestResult<Stored> {
    let row = stored(pool, event).await?;
    assert_eq!(row.outcome, outcome, "stored outcome of {event}");
    assert_eq!(row.effects, effects, "stored effects of {event}");
    Ok(row)
}

// ---------------------------------------------------------------------------
// Prepare, then another option changes, then execute
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn add_leaves_an_option_completed_by_another_event_after_prepare() -> TestResult {
    isolated(add_while_another_event_completes).await
}

async fn add_while_another_event_completes(pool: PgPool) -> TestResult {
    let fx = Fixture::new(&pool, &[UNRELATED]).await;
    let panel = catalogue(PanelMode::Button, false, "add-while-b-completes");
    let (a, b) = (click("event-a", "alpha"), click("event-b", "bravo"));
    let mut prepared = admit(&fx.runtime, &a, &panel).await?;
    assert_eq!(prepared.event.desired_role_ids, [ROLE_A]);
    assert!(prepared.event.pre_mutation_role_ids.is_empty());

    // B is prepared, executed and settled by its own event while A waits.
    assert_eq!(
        complete(&fx.runtime, &b, &panel).await?,
        SettledOutcome::Assigned
    );
    assert_eq!(fx.new_mutations(), [put(ROLE_B)]);

    let outcome = finish(&fx.runtime, &mut prepared, &panel).await?;
    assert_eq!(outcome, SettledOutcome::Assigned);
    assert_eq!(
        fx.new_mutations(),
        [put(ROLE_A)],
        "event A converges option A only; B belongs to event B"
    );
    fx.assert_held(&[ROLE_A, ROLE_B, UNRELATED]);

    let a_row = assert_audit(&pool, "event-a", "assigned", clean_add(ROLE_A)).await?;
    assert_eq!(a_row.desired, [ROLE_A], "admitted intent is immutable");
    assert!(a_row.before.is_empty(), "admitted baseline is immutable");
    assert_audit(&pool, "event-b", "assigned", clean_add(ROLE_B)).await?;
    assert_eq!(receipt_roles(&pool, "event-a").await?, [ROLE_A]);
    assert_eq!(receipt_roles(&pool, "event-b").await?, [ROLE_B]);
    assert!(matches!(
        fx.runtime.prepare(&a, &panel).await,
        Ok(Admission::Duplicate)
    ));
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn add_does_not_restore_an_option_removed_after_prepare() -> TestResult {
    isolated(add_while_another_option_is_revoked).await
}

async fn add_while_another_option_is_revoked(pool: PgPool) -> TestResult {
    let fx = Fixture::new(&pool, &[ROLE_B, UNRELATED]).await;
    let panel = catalogue(PanelMode::Button, false, "add-while-b-revoked");
    let a = click("event-a", "alpha");
    let mut prepared = admit(&fx.runtime, &a, &panel).await?;
    fx.guild.revoke(ROLE_B);

    assert_eq!(
        finish(&fx.runtime, &mut prepared, &panel).await?,
        SettledOutcome::Assigned
    );
    assert_eq!(
        fx.new_mutations(),
        [put(ROLE_A)],
        "an add never restores an option someone else removed"
    );
    fx.assert_held(&[ROLE_A, UNRELATED]);
    assert_audit(&pool, "event-a", "assigned", clean_add(ROLE_A)).await?;
    assert_eq!(receipt_roles(&pool, "event-a").await?, [ROLE_A]);
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn explicit_remove_leaves_an_option_added_after_prepare() -> TestResult {
    isolated(remove_while_another_option_is_granted).await
}

async fn remove_while_another_option_is_granted(pool: PgPool) -> TestResult {
    let fx = Fixture::new(&pool, &[ROLE_A, UNRELATED]).await;
    let panel = catalogue(PanelMode::Reaction, false, "remove-while-b-granted");
    let a = react("event-a", "alpha", true);
    let mut prepared = admit(&fx.runtime, &a, &panel).await?;
    assert!(prepared.event.desired_role_ids.is_empty());
    assert_eq!(prepared.event.pre_mutation_role_ids, [ROLE_A]);
    fx.guild.grant(ROLE_B);

    assert_eq!(
        finish(&fx.runtime, &mut prepared, &panel).await?,
        SettledOutcome::Removed
    );
    assert_eq!(
        fx.new_mutations(),
        [delete(ROLE_A)],
        "removing A never removes the option B that appeared meanwhile"
    );
    fx.assert_held(&[ROLE_B, UNRELATED]);
    let row = assert_audit(&pool, "event-a", "removed", clean_remove(ROLE_A)).await?;
    assert!(row.desired.is_empty());
    assert_eq!(row.before, [ROLE_A]);
    assert_eq!(receipt_roles(&pool, "event-a").await?, [ROLE_A]);
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn explicit_remove_does_not_restore_an_option_removed_after_prepare() -> TestResult {
    isolated(remove_while_another_option_is_revoked).await
}

async fn remove_while_another_option_is_revoked(pool: PgPool) -> TestResult {
    let fx = Fixture::new(&pool, &[ROLE_A, ROLE_B, UNRELATED]).await;
    let panel = catalogue(PanelMode::Reaction, false, "remove-while-b-revoked");
    let a = react("event-a", "alpha", true);
    let mut prepared = admit(&fx.runtime, &a, &panel).await?;
    fx.guild.revoke(ROLE_B);

    assert_eq!(
        finish(&fx.runtime, &mut prepared, &panel).await?,
        SettledOutcome::Removed
    );
    assert_eq!(
        fx.new_mutations(),
        [delete(ROLE_A)],
        "a remove never restores an option someone else removed"
    );
    fx.assert_held(&[UNRELATED]);
    assert_audit(&pool, "event-a", "removed", clean_remove(ROLE_A)).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn already_held_noop_stays_a_noop_for_an_option_added_after_prepare() -> TestResult {
    isolated(held_noop_while_another_option_is_granted).await
}

async fn held_noop_while_another_option_is_granted(pool: PgPool) -> TestResult {
    let fx = Fixture::new(&pool, &[ROLE_A, UNRELATED]).await;
    let panel = catalogue(PanelMode::Button, false, "held-noop-while-b-granted");
    let a = click("event-a", "alpha");
    let mut prepared = admit(&fx.runtime, &a, &panel).await?;
    assert_eq!(prepared.plan.outcome, SettledOutcome::AlreadyHeld);
    fx.guild.grant(ROLE_B);

    assert_eq!(
        ok(fx.runtime.execute(&mut prepared, &panel).await)?,
        Execution::Applied
    );
    assert_eq!(
        fx.new_mutations(),
        Vec::<String>::new(),
        "a no-op for A sends nothing and never deletes B"
    );
    assert_eq!(
        ok(fx
            .runtime
            .settle(&mut prepared, &panel, Execution::Applied)
            .await)?,
        SettledOutcome::AlreadyHeld
    );
    fx.assert_held(&[ROLE_A, ROLE_B, UNRELATED]);
    let row = assert_audit(&pool, "event-a", "already_held", AuditEffects::default()).await?;
    assert_eq!(row.desired, [ROLE_A]);
    assert_eq!(row.before, [ROLE_A]);
    assert!(receipt_roles(&pool, "event-a").await?.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn already_absent_noop_stays_a_noop_for_an_option_added_after_prepare() -> TestResult {
    isolated(absent_noop_while_another_option_is_granted).await
}

async fn absent_noop_while_another_option_is_granted(pool: PgPool) -> TestResult {
    let fx = Fixture::new(&pool, &[UNRELATED]).await;
    let panel = catalogue(PanelMode::Reaction, false, "absent-noop-while-b-granted");
    let a = react("event-a", "alpha", true);
    let mut prepared = admit(&fx.runtime, &a, &panel).await?;
    assert_eq!(prepared.plan.outcome, SettledOutcome::AlreadyAbsent);
    fx.guild.grant(ROLE_B);

    assert_eq!(
        finish(&fx.runtime, &mut prepared, &panel).await?,
        SettledOutcome::AlreadyAbsent
    );
    assert_eq!(
        fx.new_mutations(),
        Vec::<String>::new(),
        "removing an absent A sends nothing and never deletes B"
    );
    fx.assert_held(&[ROLE_B, UNRELATED]);
    assert_audit(&pool, "event-a", "already_absent", AuditEffects::default()).await?;
    assert!(receipt_roles(&pool, "event-a").await?.is_empty());
    Ok(())
}

// ---------------------------------------------------------------------------
// Settlement judges the audited option, not the whole panel
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn settlement_ignores_an_option_added_after_execution() -> TestResult {
    isolated(settle_after_another_option_is_granted).await
}

async fn settle_after_another_option_is_granted(pool: PgPool) -> TestResult {
    let fx = Fixture::new(&pool, &[UNRELATED]).await;
    let panel = catalogue(PanelMode::Button, false, "settle-b-granted");
    let a = click("event-a", "alpha");
    let mut prepared = admit(&fx.runtime, &a, &panel).await?;
    let execution = ok(fx.runtime.execute(&mut prepared, &panel).await)?;
    assert_eq!(execution, Execution::Applied);
    assert_eq!(fx.new_mutations(), [put(ROLE_A)]);
    fx.guild.grant(ROLE_B);

    let settled = fx.runtime.settle(&mut prepared, &panel, execution).await;
    assert!(
        matches!(settled, Ok(SettledOutcome::Assigned)),
        "A is held, so A settles whatever B did meanwhile; got {settled:?}"
    );
    assert!(fx.new_mutations().is_empty());
    fx.assert_held(&[ROLE_A, ROLE_B, UNRELATED]);
    assert_audit(&pool, "event-a", "assigned", clean_add(ROLE_A)).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn settlement_ignores_an_option_removed_after_execution() -> TestResult {
    isolated(settle_after_another_option_is_revoked).await
}

async fn settle_after_another_option_is_revoked(pool: PgPool) -> TestResult {
    let fx = Fixture::new(&pool, &[ROLE_B, UNRELATED]).await;
    let panel = catalogue(PanelMode::Button, false, "settle-b-revoked");
    let a = click("event-a", "alpha");
    let mut prepared = admit(&fx.runtime, &a, &panel).await?;
    let execution = ok(fx.runtime.execute(&mut prepared, &panel).await)?;
    assert_eq!(fx.new_mutations(), [put(ROLE_A)]);
    fx.guild.revoke(ROLE_B);

    let settled = fx.runtime.settle(&mut prepared, &panel, execution).await;
    assert!(
        matches!(settled, Ok(SettledOutcome::Assigned)),
        "A is held, so A settles whatever B did meanwhile; got {settled:?}"
    );
    assert!(fx.new_mutations().is_empty());
    fx.assert_held(&[ROLE_A, UNRELATED]);
    assert_audit(&pool, "event-a", "assigned", clean_add(ROLE_A)).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn settlement_still_refuses_when_the_audited_option_reverted_unchanged() -> TestResult {
    isolated(settle_after_the_audited_option_reverts).await
}

async fn settle_after_the_audited_option_reverts(pool: PgPool) -> TestResult {
    let fx = Fixture::new(&pool, &[UNRELATED]).await;
    let panel = catalogue(PanelMode::Button, false, "settle-a-reverted");
    let a = click("event-a", "alpha");
    let mut prepared = admit(&fx.runtime, &a, &panel).await?;
    let execution = ok(fx.runtime.execute(&mut prepared, &panel).await)?;
    fx.guild.revoke(ROLE_A);

    assert!(
        fx.runtime
            .settle(&mut prepared, &panel, execution)
            .await
            .is_err(),
        "settlement never reports success for an option that is not at its target"
    );
    assert_eq!(stored(&pool, "event-a").await?.outcome, "processing");
    assert_eq!(
        fx.new_mutations(),
        [put(ROLE_A)],
        "settlement sends nothing"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Compensation is scoped to the audited option
// ---------------------------------------------------------------------------

/// One refused or ambiguous mutation of A while B changes at the same moment.
#[derive(Clone, Copy)]
struct Compensation {
    id: &'static str,
    /// Explicit remove (reaction) instead of add (button).
    remove: bool,
    /// Discord performed A's change although the reply was a failure.
    applied: bool,
    /// B is granted at that moment; otherwise B is held and then revoked.
    b_granted: bool,
}

async fn compensation(pool: PgPool, case: Compensation) -> TestResult {
    let mut held = vec![UNRELATED];
    if case.remove {
        held.push(ROLE_A);
    }
    if !case.b_granted {
        held.push(ROLE_B);
    }
    let fx = Fixture::new(&pool, &held).await;
    let mode = if case.remove {
        PanelMode::Reaction
    } else {
        PanelMode::Button
    };
    let panel = catalogue(mode, false, case.id);
    let a = if case.remove {
        react("event-a", "alpha", true)
    } else {
        click("event-a", "alpha")
    };
    let mut prepared = admit(&fx.runtime, &a, &panel).await?;
    let change = if case.b_granted {
        Change::Grant(ROLE_B)
    } else {
        Change::Revoke(ROLE_B)
    };
    fx.guild.fail_next(
        if case.remove { "DELETE" } else { "PUT" },
        ROLE_A,
        Fault {
            status: if case.applied { 503 } else { 403 },
            applied: case.applied,
            concurrent: vec![change],
        },
    );

    let execution = ok(fx.runtime.execute(&mut prepared, &panel).await)?;
    assert_eq!(execution, Execution::Compensated);
    let sent = if case.remove {
        delete(ROLE_A)
    } else {
        put(ROLE_A)
    };
    let restored = if case.remove {
        put(ROLE_A)
    } else {
        delete(ROLE_A)
    };
    let expected = if case.applied {
        vec![sent, restored]
    } else {
        vec![sent]
    };
    assert_eq!(
        fx.new_mutations(),
        expected,
        "compensation undoes A's own change and never touches B"
    );
    assert_eq!(
        ok(fx.runtime.settle(&mut prepared, &panel, execution).await)?,
        SettledOutcome::Rejected
    );

    let mut final_roles = vec![UNRELATED];
    if case.remove {
        final_roles.push(ROLE_A);
    }
    if case.b_granted {
        final_roles.push(ROLE_B);
    }
    fx.assert_held(&final_roles);

    let effects = match (case.remove, case.applied) {
        (false, false) => AuditEffects {
            attempted_added_role_ids: ids(&[ROLE_A]),
            ..AuditEffects::default()
        },
        (false, true) => AuditEffects {
            attempted_added_role_ids: ids(&[ROLE_A]),
            attempted_removed_role_ids: ids(&[ROLE_A]),
            compensated_removed_role_ids: ids(&[ROLE_A]),
            ..AuditEffects::default()
        },
        (true, false) => AuditEffects {
            attempted_removed_role_ids: ids(&[ROLE_A]),
            ..AuditEffects::default()
        },
        (true, true) => AuditEffects {
            attempted_removed_role_ids: ids(&[ROLE_A]),
            attempted_added_role_ids: ids(&[ROLE_A]),
            compensated_added_role_ids: ids(&[ROLE_A]),
            ..AuditEffects::default()
        },
    };
    let row = assert_audit(&pool, "event-a", "rejected", effects).await?;
    assert_eq!(row.code.as_deref(), Some("compensated"));
    assert!(
        !row.pending,
        "a received failure status is not a lost reply"
    );
    assert!(!mentions(&row.effects, ROLE_B));
    assert!(receipt_roles(&pool, "event-a")
        .await?
        .iter()
        .all(|role| role == ROLE_A));
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn compensation_of_a_refused_add_leaves_an_option_added_meanwhile() -> TestResult {
    isolated(|pool| {
        compensation(
            pool,
            Compensation {
                id: "compensate-add-refused",
                remove: false,
                applied: false,
                b_granted: true,
            },
        )
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn compensation_of_an_ambiguous_add_removes_only_the_audited_option() -> TestResult {
    isolated(|pool| {
        compensation(
            pool,
            Compensation {
                id: "compensate-add-ambiguous",
                remove: false,
                applied: true,
                b_granted: true,
            },
        )
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn compensation_of_a_refused_remove_leaves_an_option_added_meanwhile() -> TestResult {
    isolated(|pool| {
        compensation(
            pool,
            Compensation {
                id: "compensate-remove-refused",
                remove: true,
                applied: false,
                b_granted: true,
            },
        )
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn compensation_of_an_ambiguous_remove_restores_only_the_audited_option() -> TestResult {
    isolated(|pool| {
        compensation(
            pool,
            Compensation {
                id: "compensate-remove-ambiguous",
                remove: true,
                applied: true,
                b_granted: true,
            },
        )
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn compensation_of_a_refused_add_does_not_restore_an_option_removed_meanwhile() -> TestResult
{
    isolated(|pool| {
        compensation(
            pool,
            Compensation {
                id: "compensate-add-b-revoked",
                remove: false,
                applied: false,
                b_granted: false,
            },
        )
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn compensation_of_an_ambiguous_remove_does_not_restore_an_option_removed_meanwhile(
) -> TestResult {
    isolated(|pool| {
        compensation(
            pool,
            Compensation {
                id: "compensate-remove-b-revoked",
                remove: true,
                applied: true,
                b_granted: false,
            },
        )
    })
    .await
}

// ---------------------------------------------------------------------------
// Lease-expiry replay is scoped to the audited option
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Replay {
    /// Admitted, nothing sent, worker lost.
    Add,
    Remove,
    NoopAdd,
    /// The change was applied, then the worker was lost before settling.
    AppliedAdd,
}

/// What the replay must leave behind.
struct Replayed {
    mutations: Vec<String>,
    outcome: &'static str,
    effects: AuditEffects,
    desired: Vec<&'static str>,
    before: Vec<&'static str>,
    held: Vec<&'static str>,
}

async fn replay(pool: PgPool, case: Replay) -> TestResult {
    let (held, mode): (&[&str], PanelMode) = match case {
        Replay::Add | Replay::AppliedAdd => (&[UNRELATED], PanelMode::Button),
        Replay::Remove => (&[ROLE_A, UNRELATED], PanelMode::Reaction),
        Replay::NoopAdd => (&[ROLE_A, UNRELATED], PanelMode::Button),
    };
    let fx = Fixture::new(&pool, held).await;
    let panel = catalogue(mode, false, "lease-replay");
    let a = match case {
        Replay::Remove => react("event-a", "alpha", true),
        _ => click("event-a", "alpha"),
    };
    let mut prepared = admit(&fx.runtime, &a, &panel).await?;
    if matches!(case, Replay::AppliedAdd) {
        assert_eq!(
            ok(fx.runtime.execute(&mut prepared, &panel).await)?,
            Execution::Applied
        );
        assert_eq!(fx.new_mutations(), [put(ROLE_A)]);
    }
    drop(prepared); // the worker is lost; its lease is left to expire
    fx.guild.grant(ROLE_B); // an independent change while nobody owns the event
    fx.advance(500);

    let service = recovery_service(fx.restarted(), &panel)?;
    assert_eq!(ok(service.recover_once().await)?, 1);

    let want = match case {
        Replay::Add => Replayed {
            mutations: vec![put(ROLE_A)],
            outcome: "assigned",
            effects: clean_add(ROLE_A),
            desired: vec![ROLE_A],
            before: vec![],
            held: vec![ROLE_A, ROLE_B, UNRELATED],
        },
        Replay::Remove => Replayed {
            mutations: vec![delete(ROLE_A)],
            outcome: "removed",
            effects: clean_remove(ROLE_A),
            desired: vec![],
            before: vec![ROLE_A],
            held: vec![ROLE_B, UNRELATED],
        },
        Replay::NoopAdd => Replayed {
            mutations: vec![],
            outcome: "already_held",
            effects: AuditEffects::default(),
            desired: vec![ROLE_A],
            before: vec![ROLE_A],
            held: vec![ROLE_A, ROLE_B, UNRELATED],
        },
        Replay::AppliedAdd => Replayed {
            mutations: vec![],
            outcome: "assigned",
            effects: clean_add(ROLE_A),
            desired: vec![ROLE_A],
            before: vec![],
            held: vec![ROLE_A, ROLE_B, UNRELATED],
        },
    };
    assert_eq!(
        fx.new_mutations(),
        want.mutations,
        "replay converges only the audited option and never touches B"
    );
    fx.assert_held(&want.held);
    let row = assert_audit(&pool, "event-a", want.outcome, want.effects).await?;
    assert_eq!(
        row.desired, want.desired,
        "replayed intent is the admitted intent"
    );
    assert_eq!(
        row.before, want.before,
        "replayed baseline is the admitted baseline"
    );
    assert!(!row.pending);
    let receipts = receipt_roles(&pool, "event-a").await?;
    assert!(receipts.iter().all(|role| role == ROLE_A), "{receipts:?}");
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn lease_expiry_replay_of_an_add_applies_only_the_audited_option() -> TestResult {
    isolated(|pool| replay(pool, Replay::Add)).await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn lease_expiry_replay_of_a_remove_applies_only_the_audited_option() -> TestResult {
    isolated(|pool| replay(pool, Replay::Remove)).await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn lease_expiry_replay_of_a_noop_stays_a_noop_for_other_options() -> TestResult {
    isolated(|pool| replay(pool, Replay::NoopAdd)).await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn lease_expiry_replay_after_an_applied_add_settles_without_touching_other_options(
) -> TestResult {
    isolated(|pool| replay(pool, Replay::AppliedAdd)).await
}

// ---------------------------------------------------------------------------
// Interrupted exchanges stay unresolved, and recovery stays scoped
// ---------------------------------------------------------------------------

/// A send of A was journaled and its reply never seen; B changes meanwhile.
#[derive(Clone, Copy)]
struct Interrupted {
    /// The unanswered request had in fact been performed.
    landed: bool,
    /// B is granted meanwhile; otherwise B is held and then revoked.
    b_granted: bool,
}

async fn interrupted(pool: PgPool, case: Interrupted) -> TestResult {
    let Interrupted { landed, b_granted } = case;
    let mut held = vec![UNRELATED];
    if !b_granted {
        held.push(ROLE_B);
    }
    let fx = Fixture::new(&pool, &held).await;
    let panel = catalogue(PanelMode::Button, false, "interrupted-exchange");
    let a = click("event-a", "alpha");
    let prepared = admit(&fx.runtime, &a, &panel).await?;
    // The process stops between journaling the send of A and learning the reply.
    let mut effects = prepared.audit.effects.clone();
    mark_attempt(&mut effects, ROLE_A, true);
    assert!(
        fx.runtime
            .store
            .checkpoint_exchange(&prepared.event, &effects, false, Some(true))
            .await?
    );
    drop(prepared);
    if landed {
        fx.guild.grant(ROLE_A); // the unanswered request had been performed
    }
    if b_granted {
        fx.guild.grant(ROLE_B); // an independent change meanwhile
    } else {
        fx.guild.revoke(ROLE_B);
    }
    fx.advance(500);

    let mut recovered = readmit(&fx.runtime, &panel).await?;
    assert!(recovered.event.exchange_pending);
    let execution = ok(fx.runtime.execute(&mut recovered, &panel).await)?;
    assert_eq!(execution, Execution::Compensated);
    let expected = if landed { vec![delete(ROLE_A)] } else { vec![] };
    assert_eq!(
        fx.new_mutations(),
        expected,
        "recovery undoes only the unanswered change to A and never touches B"
    );
    assert!(
        matches!(
            fx.runtime.settle(&mut recovered, &panel, execution).await,
            Err(RuntimeError::PendingExchange)
        ),
        "convergence of A never clears an unresolved exchange"
    );
    if b_granted {
        fx.assert_held(&[ROLE_B, UNRELATED]);
    } else {
        fx.assert_held(&[UNRELATED]);
    }

    let row = stored(&pool, "event-a").await?;
    assert_eq!(row.outcome, "processing");
    assert!(row.pending);
    assert_eq!(row.effects.unresolved_added_role_ids, [ROLE_A]);
    assert!(!mentions(&row.effects, ROLE_B));
    let receipts = receipt_roles(&pool, "event-a").await?;
    assert!(receipts.iter().all(|role| role == ROLE_A), "{receipts:?}");
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn interrupted_exchange_recovery_leaves_other_options_when_the_send_never_landed(
) -> TestResult {
    isolated(|pool| {
        interrupted(
            pool,
            Interrupted {
                landed: false,
                b_granted: true,
            },
        )
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn interrupted_exchange_recovery_removes_only_the_audited_option_when_the_send_landed(
) -> TestResult {
    isolated(|pool| {
        interrupted(
            pool,
            Interrupted {
                landed: true,
                b_granted: true,
            },
        )
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn interrupted_exchange_recovery_does_not_restore_an_option_removed_meanwhile() -> TestResult
{
    isolated(|pool| {
        interrupted(
            pool,
            Interrupted {
                landed: true,
                b_granted: false,
            },
        )
    })
    .await
}

// ---------------------------------------------------------------------------
// Controls that hold before and after the behavior change
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn select_menu_keeps_exact_set_semantics_unchanged() -> TestResult {
    isolated(select_menu_exact_set).await
}

async fn select_menu_exact_set(pool: PgPool) -> TestResult {
    let fx = Fixture::new(&pool, &[ROLE_B, UNRELATED]).await;
    let panel = catalogue(PanelMode::Select, false, "select-exact-set");

    // The submission names the whole set: B is dropped, A is added.
    assert_eq!(
        complete(&fx.runtime, &choose("select-1", &["alpha"]), &panel).await?,
        SettledOutcome::Switched
    );
    assert_eq!(fx.new_mutations(), [delete(ROLE_B), put(ROLE_A)]);
    fx.assert_held(&[ROLE_A, UNRELATED]);

    assert_eq!(
        complete(
            &fx.runtime,
            &choose("select-2", &["alpha", "bravo"]),
            &panel
        )
        .await?,
        SettledOutcome::Assigned
    );
    assert_eq!(fx.new_mutations(), [put(ROLE_B)]);
    fx.assert_held(&[ROLE_A, ROLE_B, UNRELATED]);

    assert_eq!(
        complete(&fx.runtime, &choose("select-3", &[]), &panel).await?,
        SettledOutcome::Removed
    );
    assert_eq!(fx.new_mutations(), [delete(ROLE_A), delete(ROLE_B)]);
    fx.assert_held(&[UNRELATED]);

    fx.guild.grant(ROLE_A);
    fx.guild.grant(ROLE_B);
    assert_eq!(
        complete(&fx.runtime, &choose("select-4", &["bravo"]), &panel).await?,
        SettledOutcome::Removed
    );
    assert_eq!(fx.new_mutations(), [delete(ROLE_A)]);
    fx.assert_held(&[ROLE_B, UNRELATED]);
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn exclusive_panels_keep_replacement_and_lane_rules_unchanged() -> TestResult {
    isolated(exclusive_replacement).await
}

async fn exclusive_replacement(pool: PgPool) -> TestResult {
    let fx = Fixture::new(&pool, &[ROLE_A, UNRELATED]).await;
    let panel = catalogue(PanelMode::Button, true, "exclusive-replacement");

    assert_eq!(
        complete(&fx.runtime, &click("lane-1", "bravo"), &panel).await?,
        SettledOutcome::Switched
    );
    assert_eq!(fx.new_mutations(), [delete(ROLE_A), put(ROLE_B)]);
    fx.assert_held(&[ROLE_B, UNRELATED]);
    assert_eq!(
        lane_target(&pool, &panel.id).await?,
        (Some("bravo".to_owned()), true)
    );

    assert_eq!(
        complete(&fx.runtime, &click("lane-2", "alpha"), &panel).await?,
        SettledOutcome::Switched
    );
    assert_eq!(fx.new_mutations(), [delete(ROLE_B), put(ROLE_A)]);
    fx.assert_held(&[ROLE_A, UNRELATED]);
    assert_eq!(
        lane_target(&pool, &panel.id).await?,
        (Some("alpha".to_owned()), true)
    );

    // A second option appearing under an admitted exclusive event is still
    // replaced: the single-choice invariant belongs to the lane, not the click.
    let mut prepared = admit(&fx.runtime, &click("lane-3", "alpha"), &panel).await?;
    assert_eq!(prepared.plan.outcome, SettledOutcome::AlreadyHeld);
    fx.guild.grant(ROLE_B);
    assert_eq!(
        finish(&fx.runtime, &mut prepared, &panel).await?,
        SettledOutcome::AlreadyHeld
    );
    assert_eq!(fx.new_mutations(), [delete(ROLE_B)]);
    fx.assert_held(&[ROLE_A, UNRELATED]);
    assert_eq!(
        lane_target(&pool, &panel.id).await?,
        (Some("alpha".to_owned()), true)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn ordinary_idempotent_controls_stay_precise_unchanged() -> TestResult {
    isolated(idempotent_controls).await
}

async fn idempotent_controls(pool: PgPool) -> TestResult {
    let fx = Fixture::new(&pool, &[UNRELATED]).await;
    let buttons = catalogue(PanelMode::Button, false, "idempotent-buttons");
    let first = click("ctl-1", "alpha");
    assert_eq!(
        complete(&fx.runtime, &first, &buttons).await?,
        SettledOutcome::Assigned
    );
    assert_eq!(fx.new_mutations(), [put(ROLE_A)]);
    assert_eq!(
        complete(&fx.runtime, &click("ctl-2", "alpha"), &buttons).await?,
        SettledOutcome::AlreadyHeld
    );
    assert!(fx.new_mutations().is_empty());
    assert_eq!(
        complete(&fx.runtime, &click("ctl-3", "bravo"), &buttons).await?,
        SettledOutcome::Assigned
    );
    assert_eq!(fx.new_mutations(), [put(ROLE_B)]);
    fx.assert_held(&[ROLE_A, ROLE_B, UNRELATED]);

    let calls = fx.mock.requests().len();
    assert!(matches!(
        fx.runtime.prepare(&first, &buttons).await,
        Ok(Admission::Duplicate)
    ));
    assert_eq!(fx.mock.requests().len(), calls, "a duplicate sends nothing");

    let reactions = catalogue(PanelMode::Reaction, false, "idempotent-reactions");
    assert_eq!(
        complete(&fx.runtime, &react("ctl-4", "alpha", true), &reactions).await?,
        SettledOutcome::Removed
    );
    assert_eq!(fx.new_mutations(), [delete(ROLE_A)]);
    assert_eq!(
        complete(&fx.runtime, &react("ctl-5", "alpha", true), &reactions).await?,
        SettledOutcome::AlreadyAbsent
    );
    assert!(fx.new_mutations().is_empty());
    fx.assert_held(&[ROLE_B, UNRELATED]);
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn live_hierarchy_validation_refuses_before_any_mutation_unchanged() -> TestResult {
    isolated(live_hierarchy_validation).await
}

async fn live_hierarchy_validation(pool: PgPool) -> TestResult {
    // An option above the bot is refused at admission.
    let fx = Fixture::new(&pool, &[ROLE_B, UNRELATED]).await;
    fx.guild.raise_above_bot(ROLE_A);
    let panel = catalogue(PanelMode::Button, false, "hierarchy-at-admission");
    assert!(matches!(
        fx.runtime.prepare(&click("hier-1", "alpha"), &panel).await,
        Ok(Admission::Rejected("role_hierarchy"))
    ));
    assert!(fx.new_mutations().is_empty());
    fx.assert_held(&[ROLE_B, UNRELATED]);

    // A hierarchy change after admission is refused at execution.
    let panel = catalogue(PanelMode::Button, false, "hierarchy-at-execution");
    let fx = Fixture::new(&pool, &[UNRELATED]).await;
    let mut prepared = admit(&fx.runtime, &click("hier-2", "alpha"), &panel).await?;
    fx.guild.raise_above_bot(ROLE_A);
    fx.guild.grant(ROLE_B);
    assert!(matches!(
        fx.runtime.execute(&mut prepared, &panel).await,
        Err(RuntimeError::Rest(SelfRoleRestError::Snapshot))
    ));
    assert!(fx.new_mutations().is_empty());
    fx.assert_held(&[ROLE_B, UNRELATED]);
    assert_eq!(stored(&pool, "hier-2").await?.outcome, "processing");
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn an_expired_claim_cannot_mutate_after_takeover_unchanged() -> TestResult {
    isolated(expired_claim_fence).await
}

async fn expired_claim_fence(pool: PgPool) -> TestResult {
    let fx = Fixture::new(&pool, &[UNRELATED]).await;
    let panel = catalogue(PanelMode::Button, false, "claim-fence");
    let a = click("event-a", "alpha");
    let mut stale = admit(&fx.runtime, &a, &panel).await?;
    fx.runtime.park(&mut stale).await; // stop renewing; the lease runs out
    fx.advance(500);
    let mut owner = readmit(&fx.runtime, &panel).await?;

    assert!(
        matches!(
            fx.runtime.execute(&mut stale, &panel).await,
            Err(RuntimeError::Stale)
        ),
        "the old worker is fenced out before any send"
    );
    assert!(fx.new_mutations().is_empty());

    assert_eq!(
        finish(&fx.runtime, &mut owner, &panel).await?,
        SettledOutcome::Assigned
    );
    assert_eq!(fx.new_mutations(), [put(ROLE_A)]);
    assert!(fx
        .runtime
        .settle(&mut stale, &panel, Execution::Applied)
        .await
        .is_err());
    assert_audit(&pool, "event-a", "assigned", clean_add(ROLE_A)).await?;
    Ok(())
}
