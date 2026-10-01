use super::*;
use crate::discord_test_common::{MockRest, ScriptedResponse};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use std::sync::atomic::AtomicI64;
use two_bot_core::self_roles::{event_order_for_event_id, SelfRoleOption};

const GUILD: &str = "100000000000000001";
const USER: &str = "100000000000000002";
const BOT: &str = "100000000000000003";
const NEW_ROLE: &str = "100000000000000004";
const BOT_ROLE: &str = "100000000000000005";
const OTHER: &str = "100000000000000006";
const CHANNEL: &str = "100000000000000007";
const MESSAGE: &str = "100000000000000008";
const OLD_ROLE: &str = "100000000000000009";
const NOW: i64 = 1_700_000_000_000;
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn panel(mode: PanelMode) -> SelfRolePanel {
    SelfRolePanel {
        id: "games".into(),
        channel_id: CHANNEL.into(),
        message_id: MESSAGE.into(),
        mode,
        exclusive: true,
        color: false,
        options: vec![
            SelfRoleOption {
                key: "old".into(),
                label: "Old".into(),
                role_id: OLD_ROLE.into(),
                permissions: "0".into(),
                emoji: Some("a".into()),
                description: None,
            },
            SelfRoleOption {
                key: "new".into(),
                label: "New".into(),
                role_id: NEW_ROLE.into(),
                permissions: "0".into(),
                emoji: Some("b".into()),
                description: None,
            },
        ],
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

fn snapshot(held: &[&str]) -> Vec<ScriptedResponse> {
    vec![
        ScriptedResponse::json(200, json!({"user":{"id":USER},"roles":held})),
        ScriptedResponse::json(
            200,
            json!({"user":{"id":BOT,"bot":true},"roles":[BOT_ROLE]}),
        ),
        ScriptedResponse::json(
            200,
            json!([
                {"id":GUILD,"permissions":"1024","position":0,"managed":false,"color":0},
                {"id":NEW_ROLE,"permissions":"0","position":2,"managed":false,"color":0},
                {"id":OLD_ROLE,"permissions":"0","position":3,"managed":false,"color":0},
                {"id":BOT_ROLE,"permissions":"268435456","position":10,"managed":true,"color":0},
                {"id":OTHER,"permissions":"0","position":1,"managed":false,"color":0}
            ]),
        ),
        ScriptedResponse::json(
            200,
            json!([
                {"id":CHANNEL,"guild_id":GUILD,"name":"games","permission_overwrites":[]}
            ]),
        ),
    ]
}

fn runtime(pool: &PgPool, clock: &Arc<AtomicI64>, mock: &MockRest) -> SelfRoleRuntime {
    SelfRoleRuntime {
        store: SelfRoleStore::with_test_clock(pool.clone(), 300, clock.clone()).unwrap(),
        executor: ActionExecutor::with_proxy("fixture-token".into(), Some(mock.origin())).unwrap(),
        guild_id: GUILD.into(),
        bot_id: BOT.into(),
    }
}

fn ready(admission: Admission) -> Box<PreparedSelfRole> {
    match admission {
        Admission::Ready(prepared) => prepared,
        Admission::Rejected(code) => panic!("rejected: {code}"),
        _ => panic!("expected prepared intent"),
    }
}

#[test]
fn surface_plans_preserve_option_explicit_remove_and_unrelated_roles() {
    let mut panel = panel(PanelMode::Button);
    panel.exclusive = false;
    let held = [OLD_ROLE.into(), OTHER.into()].into_iter().collect();
    let selection = Selection::Button {
        option_key: "new".into(),
    };
    let plan = selection.plan(&panel, &held).unwrap();
    assert_eq!(plan.option_key.as_deref(), Some("new"));
    assert_eq!(plan.add_role_ids, [NEW_ROLE]);
    assert!(plan.remove_role_ids.is_empty());
    panel.mode = PanelMode::Reaction;
    let remove = Selection::Reaction {
        option_key: "new".into(),
        remove: true,
    };
    assert_eq!(
        remove.plan(&panel, &held).unwrap().outcome,
        SettledOutcome::AlreadyAbsent
    );
    panel.mode = PanelMode::Select;
    let empty = Selection::Select {
        option_keys: vec![],
    };
    let plan = empty.plan(&panel, &held).unwrap();
    assert_eq!(plan.remove_role_ids, [OLD_ROLE]);
    assert!(!plan.remove_role_ids.contains(&OTHER.into()));
    panel.exclusive = true;
    let invalid = Selection::Select {
        option_keys: vec!["old".into(), "new".into()],
    };
    assert!(invalid.plan(&panel, &held).is_err());
    assert!(selection.plan(&panel, &held).is_err());
}

/// Only agent-testdb, a generated schema, and a loopback Discord double. No env
/// URLs, actual credentials, guilds, worker, staging or production resources.
#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn admission_fresh_reads_recovery_and_both_lease_renewals() -> TestResult {
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
    let schema = format!("self_role_runtime_{}_{}", std::process::id(), nonce);
    // Fixed prefix plus numeric PID/timestamp; no caller-supplied identifiers.
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
    let result = exercise(&pool).await;
    pool.close().await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await?;
    admin.close().await;
    result
}

async fn exercise(pool: &PgPool) -> TestResult {
    sqlx::raw_sql(include_str!("../../cutover/migrations/0200_self_roles.sql"))
        .execute(pool)
        .await?;
    sqlx::raw_sql(include_str!(
        "../../cutover/migrations/0201_self_role_intent_initialization.sql"
    ))
    .execute(pool)
    .await?;
    recovery_and_renewal(pool).await?;
    empty_select_recovery(pool).await?;
    reaction_partial_and_duplicate(pool).await?;
    exclusive_lane_before_reads(pool).await?;
    Ok(())
}

async fn recovery_and_renewal(pool: &PgPool) -> TestResult {
    let clock = Arc::new(AtomicI64::new(NOW));
    let mut script = snapshot(&[OLD_ROLE, OTHER]);
    script.extend(snapshot(&[NEW_ROLE, OTHER]));
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let runtime = runtime(pool, &clock, &mock);
    let panel = panel(PanelMode::Button);
    let request = request(
        "button-recovery",
        Selection::Button {
            option_key: "new".into(),
        },
    );
    let prepared = ready(runtime.prepare(&request, &panel).await.unwrap());
    assert_eq!(prepared.plan.outcome, SettledOutcome::Switched);
    assert_eq!(prepared.remaining.remove_role_ids, [OLD_ROLE]);
    assert_eq!(prepared.remaining.add_role_ids, [NEW_ROLE]);
    assert_eq!(prepared.event.pre_mutation_role_ids, [OLD_ROLE]);
    assert_eq!(prepared.event.desired_role_ids, [NEW_ROLE]);
    assert!(prepared.snapshot.member_role_ids.contains(OTHER));
    assert_eq!(
        prepared
            .panel
            .as_ref()
            .unwrap()
            .target
            .option_key
            .as_deref(),
        Some("old")
    );
    let (initialized, desired): (bool, String) = sqlx::query_as(
        "SELECT intent_initialized,desired_role_ids FROM self_role_audit WHERE event_id=$1",
    )
    .bind(&request.event_id)
    .fetch_one(pool)
    .await?;
    assert!(initialized);
    assert_eq!(desired, format!("[\"{NEW_ROLE}\"]"));
    assert!(matches!(
        runtime.prepare(&request, &panel).await.unwrap(),
        Admission::Duplicate
    ));
    assert_eq!(mock.requests().len(), 4); // duplicate did not fetch/replan
    clock.store(NOW + 200, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(250)).await;
    clock.store(NOW + 300, Ordering::SeqCst);
    assert!(prepared.owns().await.unwrap()); // BOTH renewed past original expiry
    let old_event = prepared.event.clone();
    let old_lane = prepared.panel.clone().unwrap();
    drop(prepared); // simulate crash; guards must stop renewing
    clock.store(NOW + 1_000, Ordering::SeqCst);
    assert!(!runtime.store.owns_claim(&old_event).await?);
    assert!(!runtime.store.owns_panel_claim(&old_lane).await?);
    let recovered = ready(runtime.prepare(&request, &panel).await.unwrap());
    assert!(recovered.event.recovered);
    assert_eq!(recovered.plan.outcome, SettledOutcome::Switched);
    assert_eq!(recovered.event.pre_mutation_role_ids, [OLD_ROLE]);
    assert_eq!(recovered.event.desired_role_ids, [NEW_ROLE]);
    assert!(recovered.remaining.add_role_ids.is_empty());
    assert!(recovered.remaining.remove_role_ids.is_empty());
    assert_eq!(mock.requests().len(), 8);
    assert!(mock
        .requests()
        .iter()
        .all(|request| request.method == "GET"));
    drop(recovered);
    mock.shutdown().await;
    Ok(())
}

async fn empty_select_recovery(pool: &PgPool) -> TestResult {
    let clock = Arc::new(AtomicI64::new(NOW));
    let mut script = snapshot(&[OLD_ROLE, OTHER]);
    script.extend(snapshot(&[OTHER]));
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let runtime = runtime(pool, &clock, &mock);
    let mut panel = panel(PanelMode::Select);
    panel.id = "select".into();
    let request = request(
        "select-empty",
        Selection::Select {
            option_keys: vec![],
        },
    );
    let prepared = ready(runtime.prepare(&request, &panel).await.unwrap());
    assert!(prepared.event.intent_initialized);
    assert!(prepared.event.desired_role_ids.is_empty());
    assert_eq!(prepared.plan.outcome, SettledOutcome::Removed);
    drop(prepared);
    clock.store(NOW + 500, Ordering::SeqCst);
    let recovered = ready(runtime.prepare(&request, &panel).await.unwrap());
    assert!(recovered.event.intent_initialized);
    assert!(recovered.event.desired_role_ids.is_empty());
    assert_eq!(recovered.plan.outcome, SettledOutcome::Removed);
    assert!(recovered.remaining.remove_role_ids.is_empty());
    drop(recovered);
    mock.shutdown().await;
    Ok(())
}

async fn reaction_partial_and_duplicate(pool: &PgPool) -> TestResult {
    let clock = Arc::new(AtomicI64::new(NOW));
    let mut script = vec![ScriptedResponse::json(
        200,
        json!({"id":MESSAGE,"channel_id":CHANNEL}),
    )];
    script.extend(snapshot(&[OTHER]));
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let runtime = runtime(pool, &clock, &mock);
    let mut panel = panel(PanelMode::Reaction);
    panel.id = "reaction".into();
    let request = request(
        "reaction-remove",
        Selection::Reaction {
            option_key: "new".into(),
            remove: true,
        },
    );
    let prepared = ready(runtime.prepare(&request, &panel).await.unwrap());
    assert_eq!(prepared.plan.outcome, SettledOutcome::AlreadyAbsent);
    assert!(prepared.remaining.remove_role_ids.is_empty());
    assert!(prepared.remaining.add_role_ids.is_empty());
    assert_eq!(
        mock.requests()[0].path,
        format!("/api/v10/channels/{CHANNEL}/messages/{MESSAGE}")
    );
    let mut audit = prepared.audit.clone();
    audit.outcome = prepared.plan.outcome;
    runtime.store.finish_audit(&audit, &prepared.event).await?;
    runtime
        .store
        .release_panel_claim(prepared.panel.as_ref().unwrap())
        .await?;
    drop(prepared);
    assert!(matches!(
        runtime.prepare(&request, &panel).await.unwrap(),
        Admission::Duplicate
    ));
    assert_eq!(mock.requests().len(), 5);
    assert!(mock
        .requests()
        .iter()
        .all(|request| request.method == "GET"));
    mock.shutdown().await;
    Ok(())
}

async fn exclusive_lane_before_reads(pool: &PgPool) -> TestResult {
    let clock = Arc::new(AtomicI64::new(NOW));
    let mut script = snapshot(&[OTHER]);
    script.extend(snapshot(&[OTHER]));
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let runtime = Arc::new(runtime(pool, &clock, &mock));
    let mut panel = panel(PanelMode::Button);
    panel.id = "concurrent".into();
    let first_request = request(
        "concurrent-a",
        Selection::Button {
            option_key: "old".into(),
        },
    );
    let first = ready(runtime.prepare(&first_request, &panel).await.unwrap());
    let second_request = request(
        "concurrent-b",
        Selection::Button {
            option_key: "new".into(),
        },
    );
    let contender = {
        let runtime = runtime.clone();
        let panel = panel.clone();
        tokio::spawn(async move { runtime.prepare(&second_request, &panel).await })
    };
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(mock.requests().len(), 4); // lane must precede member reads
    let mut cancelled = first.audit.clone();
    cancelled.outcome = SettledOutcome::Rejected;
    cancelled.code = Some("fixture_cancelled_before_execution".into());
    runtime.store.finish_audit(&cancelled, &first.event).await?;
    runtime
        .store
        .release_panel_claim(first.panel.as_ref().unwrap())
        .await?;
    drop(first);
    let second = ready(
        tokio::time::timeout(Duration::from_secs(3), contender)
            .await??
            .unwrap(),
    );
    assert_eq!(second.event.desired_role_ids, [NEW_ROLE]);
    assert_eq!(mock.requests().len(), 8);
    drop(second);
    mock.shutdown().await;
    Ok(())
}
