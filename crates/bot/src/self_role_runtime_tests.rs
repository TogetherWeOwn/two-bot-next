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
    sqlx::raw_sql(include_str!(
        "../../cutover/migrations/0202_self_role_compensation_phase.sql"
    ))
    .execute(pool)
    .await?;
    sqlx::raw_sql(include_str!(
        "../../cutover/migrations/0203_self_role_pending_exchange.sql"
    ))
    .execute(pool)
    .await?;
    supervised_processing_recovery(pool).await?;
    bounded_processing_sweep(pool).await?;
    durable_processing_recovery_without_redelivery(pool).await?;
    shared_reaction_dispatch_and_disabled_gate(pool).await?;
    shared_component_orchestration(pool).await?;
    dry_run_audits_without_mutation_or_target_publication(pool).await?;
    execution_and_compensation(pool).await?;
    stale_inflight_repairs_committed_target(pool).await?;
    interrupted_exchange_cannot_settle(pool).await?;
    unknown_target_is_not_empty(pool).await?;
    dry_run_refuses_recovered_mutation(pool).await?;
    recovery_and_renewal(pool).await?;
    empty_select_recovery(pool).await?;
    reaction_partial_and_duplicate(pool).await?;
    exclusive_lane_before_reads(pool).await?;
    Ok(())
}

#[test]
fn effect_snapshots_do_not_invent_unattempted_changes_or_erase_history() {
    let mut effects = AuditEffects::default();
    mark_attempt(&mut effects, OLD_ROLE, false);
    mark_attempt(&mut effects, NEW_ROLE, true);
    mark_attempt(&mut effects, NEW_ROLE, true);
    assert_eq!(effects.attempted_added_role_ids, [NEW_ROLE]);
    assert_eq!(effects.unresolved_added_role_ids, [NEW_ROLE]);
    let held = [NEW_ROLE.into(), OTHER.into()].into_iter().collect();
    observe_effects(&mut effects, &[OLD_ROLE.into()], &held);
    assert_eq!(effects.added_role_ids, [NEW_ROLE]);
    assert_eq!(effects.removed_role_ids, [OLD_ROLE]);
    assert!(effects.unresolved_added_role_ids.is_empty());
    push_role(&mut effects.compensated_added_role_ids, OLD_ROLE);
    push_role(&mut effects.compensated_removed_role_ids, NEW_ROLE);
    let held = [OLD_ROLE.into(), OTHER.into()].into_iter().collect();
    observe_effects(&mut effects, &[OLD_ROLE.into()], &held);
    assert!(effects.added_role_ids.is_empty());
    assert!(effects.removed_role_ids.is_empty());
    assert_eq!(effects.attempted_added_role_ids, [NEW_ROLE]);
    assert_eq!(effects.compensated_added_role_ids, [OLD_ROLE]);
    assert_eq!(effects.compensated_removed_role_ids, [NEW_ROLE]);
    // Differences on other roles do not become this event's observed effects.
    observe_effects(&mut effects, &[OTHER.into()], &held);
    assert!(!effects.added_role_ids.contains(&OLD_ROLE.into()));
}

async fn supervised_processing_recovery(pool: &PgPool) -> TestResult {
    use crate::{
        jobs::{self, ErrorClass},
        self_role_handlers::{SelfRoleService, RECOVERY_JOB_NAME},
        website_jobs,
    };
    use two_bot_core::self_roles::SelfRoleGates;

    for case in ["success", "pending", "shutdown", "timeout", "stopped"] {
        let clock = Arc::new(AtomicI64::new(NOW));
        let held = if case == "success" || case == "pending" {
            vec![OLD_ROLE, OTHER]
        } else {
            vec![OTHER]
        };
        let pending = matches!(case, "pending" | "timeout" | "stopped");
        let mut script = snapshot(&held);
        if case == "timeout" {
            script.push(
                ScriptedResponse::json(200, json!({"user":{"id":USER},"roles":held}))
                    .delayed(Duration::from_secs(5)),
            );
        } else if case != "stopped" {
            script.extend(snapshot(&held)); // recovered admission
            script.extend(snapshot(&held)); // execution
            if case == "shutdown" {
                script.push(ScriptedResponse::status(204).delayed(Duration::from_secs(5)));
            } else if case == "success" {
                script.push(ScriptedResponse::status(204)); // DELETE old
                script.extend(snapshot(&[OTHER]));
                script.push(ScriptedResponse::status(204)); // PUT new
                script.extend(snapshot(&[NEW_ROLE, OTHER]));
                script.extend(snapshot(&[NEW_ROLE, OTHER])); // settlement
            }
        }
        let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
        let feature = runtime(pool, &clock, &mock);
        let mut panel = panel(PanelMode::Button);
        panel.id = format!("supervised-recovery-{case}");
        let request = request(
            &panel.id,
            Selection::Button {
                option_key: "new".into(),
            },
        );
        let prepared = ready(feature.prepare(&request, &panel).await.unwrap());
        if pending {
            let mut effects = AuditEffects::default();
            mark_attempt(&mut effects, NEW_ROLE, true);
            assert!(
                feature
                    .store
                    .checkpoint_exchange(&prepared.event, &effects, false, Some(true))
                    .await?
            );
        }
        drop(prepared);
        clock.store(NOW + 500, Ordering::SeqCst);
        let service = Arc::new(
            SelfRoleService::new(
                feature,
                SelfRoleGates {
                    panels: vec![panel.clone()],
                    dry_run: false,
                },
                &[GUILD.to_owned()].into_iter().collect(),
            )
            .unwrap(),
        );
        let mut job = service.recovery_job();
        job.startup_jitter = Duration::ZERO;
        job.cadence = Duration::from_secs(600); // one attempt, no retries in fixture
        if case == "timeout" {
            job.timeout = Duration::from_secs(2);
        }
        let status = jobs::statuses(&[RECOVERY_JOB_NAME], false);
        let (stop, _) = tokio::sync::watch::channel(case == "stopped");
        let http_shutdown = stop.subscribe();
        let owner = tokio::spawn(website_jobs::serve_jobs(
            vec![job],
            status.clone(),
            stop.clone(),
            async move {
                crate::server::shutdown_requested(http_shutdown).await;
                Ok(())
            },
        ));
        if case == "shutdown" {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !mock.requests().iter().any(|r| r.method == "PUT") {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
        } else if case != "stopped" {
            tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    let current = status.read().await[RECOVERY_JOB_NAME].clone();
                    if case == "timeout" {
                        if current.last_error_class == Some(ErrorClass::Timeout) {
                            break;
                        }
                    } else {
                        assert_eq!(current.last_error_class, None);
                        if current.last_success.is_some() {
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
        }
        stop.send_replace(true);
        tokio::time::timeout(Duration::from_secs(2), owner).await???;
        let current = status.read().await[RECOVERY_JOB_NAME].clone();
        assert!(!current.running);
        if case == "stopped" {
            assert!(current.last_start.is_none());
        }
        if case == "timeout" {
            assert_eq!(current.last_error_class, Some(ErrorClass::Timeout));
            assert_eq!(current.consecutive_failures, 1);
        }
        let row: (String, bool, i32, String, String, String) = sqlx::query_as(
            "SELECT outcome,exchange_pending,claim_generation,desired_role_ids,attempted_added_role_ids,unresolved_added_role_ids FROM self_role_audit WHERE event_id=$1",
        ).bind(&request.event_id).fetch_one(pool).await?;
        assert_eq!(
            row.0,
            if case == "success" {
                "switched"
            } else {
                "processing"
            }
        );
        assert_eq!(row.1, case != "success");
        assert_eq!(row.2, if case == "stopped" { 1 } else { 2 });
        assert_eq!(serde_json::from_str::<Vec<String>>(&row.3)?, [NEW_ROLE]);
        assert_eq!(serde_json::from_str::<Vec<String>>(&row.4)?, [NEW_ROLE]);
        if case != "success" {
            assert_eq!(serde_json::from_str::<Vec<String>>(&row.5)?, [NEW_ROLE]);
        }
        let (target, committed): (Option<String>, bool) = sqlx::query_as(
            "SELECT latest_option_key,target_committed FROM self_role_panel_claims WHERE guild_id=$1 AND member_id=$2 AND panel_id=$3",
        ).bind(GUILD).bind(USER).bind(&panel.id).fetch_one(pool).await?;
        assert!(committed);
        assert_eq!(
            target.as_deref(),
            if case == "success" {
                Some("new")
            } else if case == "pending" {
                Some("old")
            } else {
                None
            }
        );
        let calls = mock.requests();
        let methods: Vec<_> = calls
            .iter()
            .filter(|r| r.method != "GET")
            .map(|r| r.method.as_str())
            .collect();
        assert_eq!(
            methods,
            if case == "success" {
                vec!["DELETE", "PUT"]
            } else if case == "shutdown" {
                vec!["PUT"]
            } else {
                vec![]
            }
        );
        // Changing the DB clock while the old lease is still live would expose
        // any renewal keeper that escaped cancellation/join of its job owner.
        let expiry: (Option<i64>, i64) = sqlx::query_as(
            "SELECT (extract(epoch FROM a.processing_expires_at)*1000)::bigint,(extract(epoch FROM p.processing_expires_at)*1000)::bigint FROM self_role_audit a JOIN self_role_panel_claims p USING(guild_id,member_id,panel_id) WHERE a.event_id=$1",
        ).bind(&request.event_id).fetch_one(pool).await?;
        clock.store(NOW + 600, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(250)).await;
        let after: (Option<i64>, i64) = sqlx::query_as(
            "SELECT (extract(epoch FROM a.processing_expires_at)*1000)::bigint,(extract(epoch FROM p.processing_expires_at)*1000)::bigint FROM self_role_audit a JOIN self_role_panel_claims p USING(guild_id,member_id,panel_id) WHERE a.event_id=$1",
        ).bind(&request.event_id).fetch_one(pool).await?;
        assert_eq!(expiry, after);
        assert_eq!(calls.len(), mock.requests().len());
        mock.shutdown().await;
    }
    Ok(())
}

async fn bounded_processing_sweep(pool: &PgPool) -> TestResult {
    use crate::self_role_handlers::SelfRoleService;
    use two_bot_core::self_roles::SelfRoleGates;

    for (case, panel_count, rows_per_panel, expected) in
        [("panels", 10, 1, vec![8, 1, 1]), ("rows", 1, 5, vec![4, 1])]
    {
        let clock = Arc::new(AtomicI64::new(NOW));
        let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
        let feature = runtime(pool, &clock, &mock);
        let mut panels = vec![];
        for i in 0..panel_count {
            let mut panel = panel(PanelMode::Button);
            panel.id = format!("bounded-sweep-{case}-{i}");
            for n in 0..rows_per_panel {
                let audit = SelfRoleAudit {
                    event_id: format!("{}-{n}", panel.id),
                    event_order: Some(format!("{n:04}")),
                    guild_id: GUILD.into(),
                    panel_id: panel.id.clone(),
                    member_id: USER.into(),
                    source_id: MESSAGE.into(),
                    option_key: Some("new".into()),
                    role_id: Some(NEW_ROLE.into()),
                    source: PanelMode::Button,
                    operation: RoleOperation::Add,
                    outcome: SettledOutcome::Rejected,
                    code: None,
                    reason: None,
                    effects: AuditEffects::default(),
                    desired_role_ids: vec![],
                    pre_mutation_role_ids: vec![],
                };
                assert!(feature.store.claim_pending_audit(&audit).await?.is_some());
            }
            panels.push(panel);
        }
        clock.store(NOW + 500, Ordering::SeqCst);
        let service = SelfRoleService::new(
            feature,
            SelfRoleGates {
                panels,
                dry_run: false,
            },
            &[GUILD.to_owned()].into_iter().collect(),
        )
        .unwrap();
        for count in expected {
            assert_eq!(service.recover_once().await.unwrap(), count);
        }
        assert_eq!(service.recover_once().await.unwrap(), 0);
        assert!(mock.requests().is_empty()); // uninitialized rows refuse before REST
        mock.shutdown().await;
    }
    Ok(())
}

async fn durable_processing_recovery_without_redelivery(pool: &PgPool) -> TestResult {
    use crate::self_role_handlers::SelfRoleService;
    use two_bot_core::self_roles::SelfRoleGates;

    for case in [
        "target",
        "empty",
        "pending",
        "dry-pending",
        "compensating",
        "uninitialized",
        "uninitialized-pending",
        "uninitialized-effects",
    ] {
        let uninitialized = case.starts_with("uninitialized");
        let inconsistent = uninitialized && case != "uninitialized";
        let clock = Arc::new(AtomicI64::new(NOW));
        let pending = case == "pending" || case == "dry-pending";
        let dry_run = case == "dry-pending";
        let empty = case == "empty";
        let compensating = case == "compensating";
        let mut script = if uninitialized {
            vec![ScriptedResponse::status(500)]
        } else {
            snapshot(&[OLD_ROLE, OTHER])
        };
        if !uninitialized {
            let held = if compensating {
                vec![OTHER]
            } else {
                vec![OLD_ROLE, OTHER]
            };
            script.extend(snapshot(&held)); // newly claimed admission
            if !dry_run {
                script.extend(snapshot(&held)); // execution revalidation
                if !pending {
                    script.push(ScriptedResponse::status(204)); // remove old, or restore old
                    if !empty && !compensating {
                        script.extend(snapshot(&[OTHER]));
                        script.push(ScriptedResponse::status(204)); // add new
                    }
                    let final_held = if empty {
                        vec![OTHER]
                    } else if compensating {
                        vec![OLD_ROLE, OTHER]
                    } else {
                        vec![NEW_ROLE, OTHER]
                    };
                    script.extend(snapshot(&final_held));
                    script.extend(snapshot(&final_held)); // final settlement
                }
            }
        }
        let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
        let feature = runtime(pool, &clock, &mock);
        let mut panel = panel(if empty {
            PanelMode::Select
        } else {
            PanelMode::Button
        });
        panel.id = format!("durable-recovery-{case}");
        let request = request(
            &panel.id,
            if empty {
                Selection::Select {
                    option_keys: vec![],
                }
            } else {
                Selection::Button {
                    option_key: "new".into(),
                }
            },
        );
        let admission = feature.prepare(&request, &panel).await;
        if uninitialized {
            assert!(admission.is_err());
            if inconsistent {
                // Inconsistent durable evidence must not become a terminal
                // rejection merely because initialization is absent.
                sqlx::query("UPDATE self_role_audit SET exchange_pending=$2,added_role_ids=$3 WHERE event_id=$1")
                    .bind(&request.event_id)
                    .bind(case == "uninitialized-pending")
                    .bind(if case == "uninitialized-effects" { serde_json::to_string(&[NEW_ROLE])? } else { "[]".into() })
                    .execute(pool).await?;
            }
        } else {
            let prepared = ready(admission.unwrap());
            if pending || compensating {
                let mut effects = AuditEffects::default();
                if pending {
                    mark_attempt(&mut effects, NEW_ROLE, true);
                } else {
                    mark_attempt(&mut effects, OLD_ROLE, false);
                    push_role(&mut effects.removed_role_ids, OLD_ROLE);
                    clear_unresolved(&mut effects, OLD_ROLE, false);
                }
                assert!(
                    feature
                        .store
                        .checkpoint_exchange(&prepared.event, &effects, compensating, Some(pending))
                        .await?
                );
            }
            drop(prepared); // crash; original request is never re-delivered
        }
        clock.store(NOW + 500, Ordering::SeqCst);
        let service = SelfRoleService::new(
            feature,
            SelfRoleGates {
                panels: vec![panel.clone()],
                dry_run,
            },
            &[GUILD.to_owned()].into_iter().collect(),
        )
        .unwrap();
        assert_eq!(service.recover_once().await.unwrap(), 1);
        assert_eq!(service.recover_once().await.unwrap(), 0); // terminal or renewed expiry
        let row: (String, Option<String>, bool, String, String, i32) = sqlx::query_as(
            "SELECT outcome,code,exchange_pending,desired_role_ids,unresolved_added_role_ids,claim_generation
             FROM self_role_audit WHERE event_id=$1",
        ).bind(&request.event_id).fetch_one(pool).await?;
        assert_eq!(row.5, 2);
        if pending {
            assert_eq!(row.0, "processing");
            assert!(row.2);
            assert_eq!(serde_json::from_str::<Vec<String>>(&row.4)?, [NEW_ROLE]);
        } else if uninitialized {
            assert_eq!(
                row.0,
                if inconsistent {
                    "processing"
                } else {
                    "rejected"
                }
            );
            assert_eq!(
                row.1.as_deref(),
                if inconsistent {
                    None
                } else {
                    Some("interrupted_before_intent")
                }
            );
            assert_eq!(row.2, case == "uninitialized-pending");
            if case == "uninitialized-effects" {
                let (evidence,): (String,) =
                    sqlx::query_as("SELECT added_role_ids FROM self_role_audit WHERE event_id=$1")
                        .bind(&request.event_id)
                        .fetch_one(pool)
                        .await?;
                assert_eq!(serde_json::from_str::<Vec<String>>(&evidence)?, [NEW_ROLE]);
            }
            assert_eq!(mock.requests().len(), 1); // no recovery REST calls
        } else {
            assert_eq!(
                row.0,
                if compensating {
                    "rejected"
                } else if empty {
                    "removed"
                } else {
                    "switched"
                }
            );
            assert_eq!(
                row.1.as_deref(),
                if compensating {
                    Some("compensated")
                } else {
                    None
                }
            );
            assert!(!row.2);
        }
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&row.3)?,
            if empty || uninitialized {
                vec![]
            } else {
                vec![NEW_ROLE.to_owned()]
            }
        );
        let calls = mock.requests();
        let mutations: Vec<_> = calls.iter().filter(|r| r.method != "GET").collect();
        if pending || uninitialized {
            assert!(mutations.is_empty());
        } else if empty {
            assert_eq!(mutations.len(), 1);
            assert_eq!(mutations[0].method, "DELETE");
        } else if compensating {
            assert_eq!(mutations.len(), 1);
            assert_eq!(mutations[0].method, "PUT");
            assert!(mutations[0].path.ends_with(OLD_ROLE));
        } else {
            assert_eq!(mutations.len(), 2);
            assert_eq!(
                (&*mutations[0].method, &*mutations[1].method),
                ("DELETE", "PUT")
            );
        }
        assert!(mutations.iter().all(|r| !r.path.ends_with(OTHER)));
        mock.shutdown().await;
    }
    Ok(())
}

async fn shared_reaction_dispatch_and_disabled_gate(pool: &PgPool) -> TestResult {
    use crate::{
        command_runtime::CommandRuntime,
        self_role_handlers::{tests::reaction, SelfRoleService},
    };
    use twilight_gateway::Event;
    use twilight_model::gateway::payload::incoming::{ReactionAdd, ReactionRemove};
    use two_bot_core::self_roles::SelfRoleGates;
    use two_bot_core::{InteractionRouter, RouterGates};

    for enabled in [false, true] {
        let clock = Arc::new(AtomicI64::new(NOW));
        let mut script = vec![];
        if enabled {
            for (held, after, mutation) in [
                (vec![OTHER], vec![NEW_ROLE, OTHER], Some(204)),
                (vec![NEW_ROLE, OTHER], vec![NEW_ROLE, OTHER], None),
                (vec![NEW_ROLE, OTHER], vec![OTHER], Some(204)),
                (vec![OTHER], vec![OTHER], None),
            ] {
                script.push(ScriptedResponse::json(
                    200,
                    json!({"id":MESSAGE,"channel_id":CHANNEL}),
                ));
                script.extend(snapshot(&held)); // fetched partial + admission
                script.extend(snapshot(&held)); // execution
                if let Some(status) = mutation {
                    script.push(ScriptedResponse::status(status));
                    script.extend(snapshot(&after)); // convergence
                }
                script.extend(snapshot(&after)); // settlement
            }
        }
        let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
        let feature = runtime(pool, &clock, &mock);
        let executor = feature.executor.clone();
        let mut panel = panel(PanelMode::Reaction);
        panel.id = format!("gateway-reaction-{enabled}");
        let service = Arc::new(
            SelfRoleService::new(
                feature,
                SelfRoleGates {
                    panels: vec![panel.clone()],
                    dry_run: false,
                },
                &[GUILD.to_owned()].into_iter().collect(),
            )
            .unwrap(),
        );
        let router = InteractionRouter::new(RouterGates {
            configured_guild: Some(GUILD.parse()?),
            scorecard: false,
            automations: false,
            announcements: false,
            moderation: false,
            tickets: false,
            self_roles: enabled,
            onboarding_picker: false,
            session_picker: false,
        });
        let command = CommandRuntime::new_with_self_roles(
            pool.clone(),
            executor,
            router,
            GUILD.parse()?,
            false,
            service,
        );
        let expected = ["assigned", "already_held", "removed", "already_absent"];
        let mut ids = HashSet::new();
        for (i, expected_outcome) in expected.into_iter().enumerate() {
            let partial = reaction(); // no cached member on either delivery
            let event = if i < 2 {
                Event::ReactionAdd(Box::new(ReactionAdd(partial)))
            } else {
                Event::ReactionRemove(Box::new(ReactionRemove(partial)))
            };
            command.dispatch(&event);
            if enabled {
                // Only a bounded fixture wait for detached dispatch; no CI polling.
                let rows = tokio::time::timeout(Duration::from_secs(8), async {
                    loop {
                        let rows: Vec<(String, String)> = sqlx::query_as(
                            "SELECT event_id,outcome FROM self_role_audit WHERE panel_id=$1 ORDER BY event_order COLLATE \"C\"",
                        ).bind(&panel.id).fetch_all(pool).await?;
                        if rows.len() == i + 1 && rows.iter().all(|r| r.1 != "processing") {
                            return Ok::<_, sqlx::Error>(rows);
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }).await??;
                let latest = rows.last().unwrap();
                assert_eq!(latest.1, expected_outcome);
                assert!(ids.insert(latest.0.clone()));
            }
        }
        let calls = mock.requests();
        if enabled {
            assert_eq!(ids.len(), 4);
            let mutations: Vec<_> = calls.iter().filter(|r| r.method != "GET").collect();
            assert_eq!(mutations.len(), 2); // duplicate add/remove are true no-ops
            assert_eq!(
                (&*mutations[0].method, &*mutations[1].method),
                ("PUT", "DELETE")
            );
            assert!(mutations
                .iter()
                .all(|r| r.path.ends_with(NEW_ROLE) && r.body.is_empty()));
            assert_eq!(
                calls
                    .iter()
                    .filter(|r| r.path.ends_with(&format!("/messages/{MESSAGE}")))
                    .count(),
                4
            );
            let (target, committed): (Option<String>, bool) = sqlx::query_as(
                "SELECT latest_option_key,target_committed FROM self_role_panel_claims WHERE guild_id=$1 AND member_id=$2 AND panel_id=$3",
            ).bind(GUILD).bind(USER).bind(&panel.id).fetch_one(pool).await?;
            assert!(target.is_none() && committed);
        } else {
            assert!(calls.is_empty());
            let (count,): (i64,) =
                sqlx::query_as("SELECT count(*) FROM self_role_audit WHERE panel_id=$1")
                    .bind(&panel.id)
                    .fetch_one(pool)
                    .await?;
            assert_eq!(count, 0);
        }
        mock.shutdown().await;
    }
    Ok(())
}

async fn shared_component_orchestration(pool: &PgPool) -> TestResult {
    use crate::{
        command_runtime::CommandRuntime,
        self_role_handlers::{tests::component, SelfRoleService},
    };
    use two_bot_core::self_roles::SelfRoleGates;
    use two_bot_core::{InteractionRouter, RouterGates};

    for (i, (enabled, dry_run, defer_status)) in [
        (true, false, 204),
        (true, true, 204),
        (true, false, 403),
        (false, false, 204),
    ]
    .into_iter()
    .enumerate()
    {
        let clock = Arc::new(AtomicI64::new(NOW));
        let mut script = vec![];
        if enabled {
            script.push(ScriptedResponse::status(defer_status));
            if defer_status == 204 {
                script.extend(snapshot(&[OLD_ROLE, OTHER]));
                if !dry_run {
                    script.extend(snapshot(&[OLD_ROLE, OTHER]));
                    script.push(ScriptedResponse::status(204));
                    script.extend(snapshot(&[OTHER]));
                    script.push(ScriptedResponse::status(204));
                    script.extend(snapshot(&[NEW_ROLE, OTHER]));
                    script.extend(snapshot(&[NEW_ROLE, OTHER]));
                }
                script.push(ScriptedResponse::json(200, json!({})));
                // Re-delivery uses the same interaction id and never fetches or mutates.
                script.push(ScriptedResponse::status(204));
                script.push(ScriptedResponse::json(200, json!({})));
            }
        }
        let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
        let feature = runtime(pool, &clock, &mock);
        let executor = feature.executor.clone();
        let mut panel = panel(PanelMode::Select);
        panel.id = format!("handler-{i}");
        let service = Arc::new(
            SelfRoleService::new(
                feature,
                SelfRoleGates {
                    panels: vec![panel.clone()],
                    dry_run,
                },
                &[GUILD.to_owned()].into_iter().collect(),
            )
            .unwrap(),
        );
        let router = InteractionRouter::new(RouterGates {
            configured_guild: Some(GUILD.parse()?),
            scorecard: false,
            automations: false,
            announcements: false,
            moderation: false,
            tickets: false,
            self_roles: enabled,
            onboarding_picker: false,
            session_picker: false,
        });
        let command = CommandRuntime::new_with_self_roles(
            pool.clone(),
            executor,
            router,
            GUILD.parse()?,
            false,
            service,
        );
        let id = 100_000_000_000_001_000 + i as u64;
        let interaction = component(&panel, id, vec!["new".into()]);
        command.on_interaction(&interaction).await;
        let row: Option<(String, Option<String>, bool)> = sqlx::query_as(
            "SELECT outcome,code,exchange_pending FROM self_role_audit WHERE event_id=$1",
        )
        .bind(id.to_string())
        .fetch_optional(pool)
        .await?;
        if enabled && defer_status == 204 {
            let row = row.unwrap();
            assert_eq!(row.0, if dry_run { "rejected" } else { "switched" });
            assert_eq!(
                row.1.as_deref(),
                if dry_run { Some("dry_run") } else { None }
            );
            assert!(!row.2);
            command.on_interaction(&interaction).await;
            let calls = mock.requests();
            let ack: serde_json::Value = serde_json::from_slice(&calls[0].body)?;
            assert_eq!(ack["type"], 5);
            assert_eq!(ack["data"]["flags"], 64);
            let mutations: Vec<_> = calls
                .iter()
                .filter(|call| call.method == "PUT" || call.method == "DELETE")
                .collect();
            if dry_run {
                assert!(mutations.is_empty());
            } else {
                assert_eq!(mutations.len(), 2);
                assert_eq!(mutations[0].method, "DELETE");
                assert_eq!(mutations[1].method, "PUT");
            }
            let reply: serde_json::Value = serde_json::from_slice(&calls.last().unwrap().body)?;
            assert!(reply["content"].as_str().unwrap().contains("already"));
        } else {
            assert!(row.is_none());
            assert_eq!(mock.requests().len(), usize::from(enabled));
        }
        mock.shutdown().await;
    }
    Ok(())
}

async fn dry_run_audits_without_mutation_or_target_publication(pool: &PgPool) -> TestResult {
    for (i, (mode, selection)) in [
        (
            PanelMode::Button,
            Selection::Button {
                option_key: "new".into(),
            },
        ),
        (
            PanelMode::Select,
            Selection::Select {
                option_keys: vec!["new".into()],
            },
        ),
        (
            PanelMode::Select,
            Selection::Select {
                option_keys: vec![],
            },
        ),
        (
            PanelMode::Reaction,
            Selection::Reaction {
                option_key: "new".into(),
                remove: false,
            },
        ),
        (
            PanelMode::Reaction,
            Selection::Reaction {
                option_key: "old".into(),
                remove: true,
            },
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let clock = Arc::new(AtomicI64::new(NOW));
        let mut script = vec![];
        if mode == PanelMode::Reaction {
            script.push(ScriptedResponse::json(
                200,
                json!({"id":MESSAGE,"channel_id":CHANNEL}),
            ));
        }
        script.extend(snapshot(&[OLD_ROLE, OTHER]));
        let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
        let runtime = runtime(pool, &clock, &mock);
        let mut panel = panel(mode);
        panel.id = format!("dry-run-{i}");
        let request = request(&format!("dry-run-{i}"), selection);
        let mut prepared = ready(runtime.prepare(&request, &panel).await.unwrap());
        let desired = prepared.event.desired_role_ids.clone();
        runtime.settle_dry_run(&mut prepared, &panel).await.unwrap();
        let row: (String, String, String, String, String, String, bool) = sqlx::query_as(
            "SELECT a.outcome,a.code,a.added_role_ids,a.attempted_added_role_ids,
             a.desired_role_ids,p.latest_option_key,p.target_committed
             FROM self_role_audit a JOIN self_role_panel_claims p
             ON a.guild_id=p.guild_id AND a.member_id=p.member_id AND a.panel_id=p.panel_id
             WHERE a.event_id=$1",
        )
        .bind(&request.event_id)
        .fetch_one(pool)
        .await?;
        assert_eq!(row.0, "rejected");
        assert_eq!(row.1, "dry_run");
        assert_eq!(row.2, "[]");
        assert_eq!(row.3, "[]");
        assert_eq!(serde_json::from_str::<Vec<String>>(&row.4)?, desired);
        assert_eq!(row.5, "old");
        assert!(row.6);
        assert!(matches!(
            runtime.prepare(&request, &panel).await.unwrap(),
            Admission::Duplicate
        ));
        assert!(mock.requests().iter().all(|call| call.method == "GET"));
        mock.shutdown().await;
    }
    Ok(())
}

async fn dry_run_refuses_recovered_mutation(pool: &PgPool) -> TestResult {
    let clock = Arc::new(AtomicI64::new(NOW));
    let mut script = snapshot(&[OLD_ROLE, OTHER]);
    script.extend(snapshot(&[OLD_ROLE, OTHER]));
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let runtime = runtime(pool, &clock, &mock);
    let mut panel = panel(PanelMode::Button);
    panel.id = "dry-run-recovery".into();
    let request = request(
        "dry-run-recovery",
        Selection::Button {
            option_key: "new".into(),
        },
    );
    let prepared = ready(runtime.prepare(&request, &panel).await.unwrap());
    let mut effects = AuditEffects::default();
    mark_attempt(&mut effects, NEW_ROLE, true);
    assert!(
        runtime
            .store
            .checkpoint_exchange(&prepared.event, &effects, false, Some(true))
            .await?
    );
    drop(prepared);
    clock.store(NOW + 301, Ordering::SeqCst);
    let mut recovered = ready(runtime.prepare(&request, &panel).await.unwrap());
    assert!(runtime
        .settle_dry_run(&mut recovered, &panel)
        .await
        .is_err());
    let row: (String, bool, String) = sqlx::query_as(
        "SELECT outcome,exchange_pending,unresolved_added_role_ids FROM self_role_audit WHERE event_id=$1",
    ).bind(&request.event_id).fetch_one(pool).await?;
    assert_eq!(row.0, "processing");
    assert!(row.1);
    assert_eq!(serde_json::from_str::<Vec<String>>(&row.2)?, [NEW_ROLE]);
    assert!(mock.requests().iter().all(|call| call.method == "GET"));
    drop(recovered);
    mock.shutdown().await;
    Ok(())
}

async fn execution_and_compensation(pool: &PgPool) -> TestResult {
    for status in [204, 403, 503] {
        let clock = Arc::new(AtomicI64::new(NOW));
        let mut script = snapshot(&[OLD_ROLE, OTHER]); // prepare
        script.extend(snapshot(&[OLD_ROLE, OTHER])); // before DELETE
        script.push(ScriptedResponse::status(204));
        script.extend(snapshot(&[OTHER])); // before PUT
        script.push(ScriptedResponse::status(status));
        if status == 204 {
            script.extend(snapshot(&[NEW_ROLE, OTHER])); // final observed target
            script.extend(snapshot(&[NEW_ROLE, OTHER])); // settlement revalidation
        } else {
            if status == 503 {
                // An ambiguous PUT may have applied. Rollback removes it before
                // restoring the old selection, and preserves attempted history.
                script.extend(snapshot(&[NEW_ROLE, OTHER]));
                script.push(ScriptedResponse::status(204));
            }
            script.extend(snapshot(&[OTHER]));
            script.push(ScriptedResponse::status(204)); // compensation PUT old
            script.extend(snapshot(&[OLD_ROLE, OTHER]));
            script.extend(snapshot(&[OLD_ROLE, OTHER])); // settlement revalidation
        }
        let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
        let runtime = runtime(pool, &clock, &mock);
        let mut panel = panel(PanelMode::Select);
        panel.id = format!("execution-{status}");
        let request = request(
            &format!("execution-{status}"),
            Selection::Select {
                option_keys: vec!["new".into()],
            },
        );
        let mut prepared = ready(runtime.prepare(&request, &panel).await.unwrap());
        let execution = runtime.execute(&mut prepared, &panel).await.unwrap();
        assert_eq!(
            execution,
            if status == 204 {
                Execution::Applied
            } else {
                Execution::Compensated
            }
        );
        assert_eq!(prepared.event.desired_role_ids, [NEW_ROLE]);
        assert_eq!(prepared.event.pre_mutation_role_ids, [OLD_ROLE]);
        assert!(prepared.snapshot.member_role_ids.contains(OTHER));
        assert!(prepared
            .audit
            .effects
            .attempted_added_role_ids
            .contains(&NEW_ROLE.into()));
        assert!(prepared
            .audit
            .effects
            .attempted_removed_role_ids
            .contains(&OLD_ROLE.into()));
        assert!(prepared.audit.effects.unresolved_added_role_ids.is_empty());
        if status == 204 {
            assert_eq!(prepared.audit.effects.added_role_ids, [NEW_ROLE]);
            assert_eq!(prepared.audit.effects.removed_role_ids, [OLD_ROLE]);
        } else {
            assert!(prepared.event.compensating);
            assert!(prepared.audit.effects.added_role_ids.is_empty());
            assert!(prepared.audit.effects.removed_role_ids.is_empty());
            assert_eq!(
                prepared.audit.effects.compensated_added_role_ids,
                [OLD_ROLE]
            );
            assert_eq!(
                prepared.audit.effects.compensated_removed_role_ids,
                if status == 503 {
                    vec![NEW_ROLE.to_owned()]
                } else {
                    vec![]
                }
            );
        }
        let mutations: Vec<_> = mock
            .requests()
            .into_iter()
            .filter(|r| r.method != "GET")
            .collect();
        assert_eq!(mutations[0].method, "DELETE");
        assert!(mutations[0].path.ends_with(OLD_ROLE));
        assert_eq!(mutations[1].method, "PUT");
        assert!(mutations[1].path.ends_with(NEW_ROLE));
        assert!(mutations
            .iter()
            .all(|r| !r.path.ends_with(OTHER) && r.body.is_empty()));
        let (phase, outcome): (bool, String) =
            sqlx::query_as("SELECT compensating,outcome FROM self_role_audit WHERE event_id=$1")
                .bind(&request.event_id)
                .fetch_one(pool)
                .await?;
        assert_eq!(phase, status != 204);
        assert_eq!(outcome, "processing"); // execute is NOT final settlement
        let settled = runtime
            .settle(&mut prepared, &panel, execution)
            .await
            .unwrap();
        assert_eq!(
            settled,
            if status == 204 {
                SettledOutcome::Switched
            } else {
                SettledOutcome::Rejected
            }
        );
        let (target, committed, outcome, pending): (Option<String>, bool, String, bool) =
            sqlx::query_as(
                "SELECT latest_option_key,target_committed,outcome,exchange_pending
             FROM self_role_panel_claims JOIN self_role_audit USING(guild_id,member_id,panel_id)
             WHERE event_id=$1",
            )
            .bind(&request.event_id)
            .fetch_one(pool)
            .await?;
        assert_eq!(
            target.as_deref(),
            Some(if status == 204 { "new" } else { "old" })
        );
        assert!(committed && !pending);
        assert_eq!(outcome, settled.as_str());
        assert!(matches!(
            runtime.prepare(&request, &panel).await.unwrap(),
            Admission::Duplicate
        ));
        drop(prepared);
        mock.shutdown().await;
    }
    compensation_restart(pool).await?;
    Ok(())
}

async fn compensation_restart(pool: &PgPool) -> TestResult {
    let clock = Arc::new(AtomicI64::new(NOW));
    let mut script = snapshot(&[OLD_ROLE, OTHER]);
    script.extend(snapshot(&[OLD_ROLE, OTHER]));
    script.push(ScriptedResponse::status(204)); // remove old
    script.extend(snapshot(&[OTHER]));
    script.push(ScriptedResponse::status(403)); // add new refused
    script.extend(snapshot(&[OTHER]));
    script.push(ScriptedResponse::status(403)); // restoring old also refused
                                                // A crash/restart must finish restoring the immutable before, NOT add new.
    script.extend(snapshot(&[OTHER])); // recovered prepare
    script.extend(snapshot(&[OTHER]));
    script.push(ScriptedResponse::status(204)); // restore old
    script.extend(snapshot(&[OLD_ROLE, OTHER]));
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let runtime = runtime(pool, &clock, &mock);
    let mut panel = panel(PanelMode::Button);
    panel.id = "compensation-restart".into();
    let request = request(
        "compensation-restart",
        Selection::Button {
            option_key: "new".into(),
        },
    );
    let mut prepared = ready(runtime.prepare(&request, &panel).await.unwrap());
    assert!(matches!(
        runtime.execute(&mut prepared, &panel).await,
        Err(RuntimeError::Rest(SelfRoleRestError::Rejected(403)))
    ));
    assert!(prepared.event.compensating);
    drop(prepared);
    clock.store(NOW + 500, Ordering::SeqCst);
    let mut recovered = ready(runtime.prepare(&request, &panel).await.unwrap());
    assert!(recovered.event.compensating);
    assert_eq!(recovered.remaining.add_role_ids, [OLD_ROLE]);
    assert_eq!(
        runtime.execute(&mut recovered, &panel).await.unwrap(),
        Execution::Compensated
    );
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.method == "PUT" && r.path.ends_with(NEW_ROLE))
            .count(),
        1
    );
    assert!(recovered
        .audit
        .effects
        .attempted_added_role_ids
        .contains(&NEW_ROLE.into()));
    assert_eq!(
        recovered.audit.effects.compensated_added_role_ids,
        [OLD_ROLE]
    );
    assert!(recovered.audit.effects.removed_role_ids.is_empty());
    drop(recovered);
    mock.shutdown().await;
    Ok(())
}

async fn stale_inflight_repairs_committed_target(pool: &PgPool) -> TestResult {
    for empty in [false, true] {
        let clock = Arc::new(AtomicI64::new(NOW));
        let mut panel = panel(PanelMode::Select);
        panel.id = format!("stale-inflight-{empty}");
        let mut old_script = snapshot(&[OTHER]);
        old_script.push(ScriptedResponse::status(204).delayed(Duration::from_secs(4)));
        let live = if empty {
            vec![OLD_ROLE, OTHER]
        } else {
            vec![OLD_ROLE, NEW_ROLE, OTHER]
        };
        let target = if empty {
            vec![OTHER]
        } else {
            vec![NEW_ROLE, OTHER]
        };
        old_script.extend(snapshot(&live)); // stale PUT applied AFTER winner settled
        old_script.push(ScriptedResponse::status(204)); // repair DELETE obsolete old
        old_script.extend(snapshot(&target));
        let old_mock = MockRest::start(old_script, ScriptedResponse::status(500)).await;
        let old_runtime = Arc::new(runtime(pool, &clock, &old_mock));
        let mut old_request = request(
            &format!("stale-old-{empty}"),
            Selection::Select {
                option_keys: vec!["old".into()],
            },
        );
        old_request.event_order = "0001".into();
        let mut old = ready(old_runtime.prepare(&old_request, &panel).await.unwrap());
        let inflight = {
            let runtime = old_runtime.clone();
            tokio::spawn(async move {
                let result = runtime.execute_step(&mut old, OLD_ROLE, true, None).await;
                (old, result)
            })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while !old_mock.requests().iter().any(|r| r.method == "PUT") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        // Database expiry while the remote mutation is in flight. A different
        // worker/executor wins; the stale worker cannot cancel Discord's call.
        clock.store(NOW + 500, Ordering::SeqCst);
        let mut new_script = snapshot(&[OTHER]);
        new_script.extend(snapshot(&[OTHER]));
        if !empty {
            new_script.push(ScriptedResponse::status(204));
            new_script.extend(snapshot(&target));
        }
        new_script.extend(snapshot(&target)); // final settlement verification
        let new_mock = MockRest::start(new_script, ScriptedResponse::status(500)).await;
        let new_runtime = runtime(pool, &clock, &new_mock);
        let mut new_request = request(
            &format!("stale-new-{empty}"),
            Selection::Select {
                option_keys: if empty { vec![] } else { vec!["new".into()] },
            },
        );
        new_request.event_order = "0002".into();
        let mut winner = ready(new_runtime.prepare(&new_request, &panel).await.unwrap());
        let applied = new_runtime.execute(&mut winner, &panel).await.unwrap();
        new_runtime
            .settle(&mut winner, &panel, applied)
            .await
            .unwrap();
        assert!(
            !inflight.is_finished(),
            "winner must settle before old response arrives"
        );
        let (mut stale, result) = tokio::time::timeout(Duration::from_secs(6), inflight).await??;
        assert!(matches!(result, Err(RuntimeError::Stale)));
        assert!(!stale.event.exchange_pending); // received late 204, retained evidence
        old_runtime
            .reconcile_stale(&mut stale, &panel)
            .await
            .unwrap();
        assert_eq!(
            panel_roles(&panel, &stale.snapshot.member_role_ids),
            if empty {
                vec![]
            } else {
                vec![NEW_ROLE.to_owned()]
            }
        );
        assert!(stale.snapshot.member_role_ids.contains(OTHER));
        let mutations: Vec<_> = old_mock
            .requests()
            .into_iter()
            .filter(|r| r.method != "GET")
            .collect();
        assert_eq!(mutations.len(), 2);
        assert_eq!(
            (&*mutations[0].method, &*mutations[1].method),
            ("PUT", "DELETE")
        );
        assert!(mutations
            .iter()
            .all(|r| r.path.ends_with(OLD_ROLE) && r.body.is_empty()));
        let (event, option, committed): (String, Option<String>, bool) = sqlx::query_as(
            "SELECT latest_event_id,latest_option_key,target_committed FROM self_role_panel_claims
             WHERE guild_id=$1 AND member_id=$2 AND panel_id=$3",
        )
        .bind(GUILD)
        .bind(USER)
        .bind(&panel.id)
        .fetch_one(pool)
        .await?;
        assert_eq!(event, new_request.event_id);
        assert_eq!(option.as_deref(), if empty { None } else { Some("new") });
        assert!(committed);
        let (outcome, code, pending, compensated): (String, String, bool, String) = sqlx::query_as(
            "SELECT outcome,code,exchange_pending,compensated_removed_role_ids
             FROM self_role_audit WHERE event_id=$1",
        )
        .bind(&old_request.event_id)
        .fetch_one(pool)
        .await?;
        assert_eq!(
            (outcome.as_str(), code.as_str()),
            ("rejected", "superseded_by_later_event")
        );
        assert!(!pending);
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&compensated)?,
            [OLD_ROLE]
        );
        drop(stale);
        drop(winner);
        old_mock.shutdown().await;
        new_mock.shutdown().await;
    }
    Ok(())
}

async fn interrupted_exchange_cannot_settle(pool: &PgPool) -> TestResult {
    let clock = Arc::new(AtomicI64::new(NOW));
    let mut script = snapshot(&[OLD_ROLE, OTHER]);
    script.extend(snapshot(&[OLD_ROLE, OTHER])); // recovered admission
    script.extend(snapshot(&[OLD_ROLE, OTHER])); // looks restored, NOT remote completion
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let runtime = runtime(pool, &clock, &mock);
    let mut panel = panel(PanelMode::Button);
    panel.id = "interrupted-exchange".into();
    let request = request(
        "interrupted-exchange",
        Selection::Button {
            option_key: "new".into(),
        },
    );
    let prepared = ready(runtime.prepare(&request, &panel).await.unwrap());
    let mut effects = prepared.audit.effects.clone();
    mark_attempt(&mut effects, NEW_ROLE, true);
    assert!(
        runtime
            .store
            .checkpoint_exchange(&prepared.event, &effects, false, Some(true))
            .await?
    );
    drop(prepared); // process stops between journal and response
    clock.store(NOW + 500, Ordering::SeqCst);
    let mut recovered = ready(runtime.prepare(&request, &panel).await.unwrap());
    assert!(recovered.event.exchange_pending);
    assert!(recovered.remaining.add_role_ids.is_empty()); // no retry of rejected target
    let result = runtime.execute(&mut recovered, &panel).await.unwrap();
    assert_eq!(result, Execution::Compensated);
    assert!(matches!(
        runtime.settle(&mut recovered, &panel, result).await,
        Err(RuntimeError::PendingExchange)
    ));
    let (outcome, pending, unresolved): (String, bool, String) = sqlx::query_as(
        "SELECT outcome,exchange_pending,unresolved_added_role_ids FROM self_role_audit WHERE event_id=$1",
    ).bind(&request.event_id).fetch_one(pool).await?;
    assert_eq!(outcome, "processing");
    assert!(pending);
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&unresolved)?,
        [NEW_ROLE]
    );
    assert!(mock.requests().iter().all(|r| r.method == "GET"));
    drop(recovered);
    mock.shutdown().await;
    Ok(())
}

async fn unknown_target_is_not_empty(pool: &PgPool) -> TestResult {
    let clock = Arc::new(AtomicI64::new(NOW));
    let mock = MockRest::start(
        snapshot(&[OLD_ROLE, NEW_ROLE, OTHER]),
        ScriptedResponse::status(500),
    )
    .await;
    let runtime = runtime(pool, &clock, &mock);
    let mut panel = panel(PanelMode::Select);
    panel.id = "unknown-target".into();
    let request = request(
        "unknown-target",
        Selection::Select {
            option_keys: vec!["new".into()],
        },
    );
    let mut prepared = ready(runtime.prepare(&request, &panel).await.unwrap());
    assert!(!prepared.panel.as_ref().unwrap().target.committed);
    clock.store(NOW + 500, Ordering::SeqCst);
    assert!(matches!(
        runtime.reconcile_stale(&mut prepared, &panel).await,
        Err(RuntimeError::InvalidSnapshot)
    ));
    assert_eq!(mock.requests().len(), 4); // no repair read/mutation to an unknown target
    drop(prepared);
    mock.shutdown().await;
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
