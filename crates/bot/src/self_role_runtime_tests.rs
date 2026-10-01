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

#[test]
fn step_receipts_use_sender_facts_not_result_or_ownership() {
    for error in [SelfRoleRestError::InvalidId, SelfRoleRestError::StaleClaim] {
        assert_eq!(step_receipt(&Err(error)), Some(ExchangeReceipt::NoSend));
    }
    for owned_after in [false, true] {
        for (status, result) in [
            (204, Ok(())),
            (403, Err(SelfRoleRestError::Rejected(403))),
            (429, Err(SelfRoleRestError::RateLimited)),
            (500, Err(SelfRoleRestError::Ambiguous)),
        ] {
            assert_eq!(
                step_receipt(&Ok(RoleExchange {
                    result,
                    owned_after,
                    response_received: true,
                    response_status: Some(status),
                })),
                Some(ExchangeReceipt::Response { status })
            );
        }
        assert_eq!(
            step_receipt(&Ok(RoleExchange {
                result: Err(SelfRoleRestError::Ambiguous),
                owned_after,
                response_received: false,
                response_status: None,
            })),
            None
        );
    }
}

#[test]
fn definite_attempt_evidence_preserves_only_prior_same_direction_uncertainty() {
    for add in [false, true] {
        for pending in [false, true] {
            for same_direction in [false, true] {
                for opposite_direction in [false, true] {
                    let mut prior = AuditEffects {
                        added_role_ids: vec![OTHER.into()],
                        removed_role_ids: vec![OLD_ROLE.into()],
                        attempted_added_role_ids: vec![OTHER.into()],
                        attempted_removed_role_ids: vec![OLD_ROLE.into()],
                        compensated_added_role_ids: vec![OLD_ROLE.into()],
                        compensated_removed_role_ids: vec![OTHER.into()],
                        unresolved_added_role_ids: vec![OTHER.into()],
                        unresolved_removed_role_ids: vec![OLD_ROLE.into()],
                    };
                    if same_direction {
                        mark_attempt(&mut prior, NEW_ROLE, add);
                    }
                    if opposite_direction {
                        mark_attempt(&mut prior, NEW_ROLE, !add);
                    }
                    let mut effects = prior.clone();
                    mark_attempt(&mut effects, NEW_ROLE, add);
                    let mut expected = effects.clone();
                    if !(pending && same_direction) {
                        clear_unresolved(&mut expected, NEW_ROLE, add);
                    }
                    resolve_attempt(&mut effects, &prior, pending, NEW_ROLE, add);
                    assert_eq!(effects, expected, "add={add}, pending={pending}");
                    // Repeating definite evidence is idempotent. History, net
                    // effects and opposite-direction uncertainty remain intact.
                    resolve_attempt(&mut effects, &prior, pending, NEW_ROLE, add);
                    assert_eq!(effects, expected);
                }
            }
        }
    }
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
    sqlx::raw_sql(include_str!(
        "../../cutover/migrations/0204_self_role_terminal_repair.sql"
    ))
    .execute(pool)
    .await?;
    sqlx::raw_sql(include_str!(
        "../../cutover/migrations/0205_self_role_exchange_receipts.sql"
    ))
    .execute(pool)
    .await?;
    sqlx::raw_sql(include_str!(
        "../../cutover/migrations/0206_self_role_exchange_baselines.sql"
    ))
    .execute(pool)
    .await?;
    processing_superseded_before_preparation(pool).await?;
    terminal_restart_repairs_committed_target(pool).await?;
    terminal_restart_refuses_unknown_or_uninitialized_target(pool).await?;
    terminal_restart_preserves_inherited_pending(pool).await?;
    terminal_restart_cancellation_preserves_journal(pool).await?;
    terminal_partial_repair_exchange_outcomes(pool).await?;
    processing_receipts_survive_generation_transfer(pool).await?;
    repeated_processing_tickets_preserve_pending(pool).await?;
    terminal_late_response_after_independent_transfer(pool).await?;
    terminal_pacing_loss_is_definite_no_send(pool).await?;
    terminal_post_journal_wait_is_definite_no_send(pool).await?;
    mixed_terminal_processing_sweep_and_dry_run(pool).await?;
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

async fn processing_superseded_before_preparation(pool: &PgPool) -> TestResult {
    use crate::self_role_handlers::SelfRoleService;
    use two_bot_core::self_roles::SelfRoleGates;

    for option in [Some("new"), None] {
        for case in ["clean", "effects", "compensating", "pending", "dry-pending"] {
            let clock = Arc::new(AtomicI64::new(NOW));
            let mut panel = panel(PanelMode::Select);
            let target = if option.is_some() {
                "selected"
            } else {
                "empty"
            };
            panel.id = format!("early-supersession-{target}-{case}");
            let pending = case.ends_with("pending");
            let dry_run = case == "dry-pending";
            let dirty = case != "clean";
            let mut script = vec![];
            if dirty && !dry_run {
                let mut held = vec![OTHER];
                if option.is_some() {
                    held.push(NEW_ROLE);
                }
                if !pending {
                    held.push(OLD_ROLE);
                }
                script.extend(snapshot(&held));
                if !pending {
                    script.push(ScriptedResponse::status(204)); // repair obsolete role only
                    held.retain(|id| *id != OLD_ROLE);
                    script.extend(snapshot(&held));
                }
            }
            let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
            let feature = runtime(pool, &clock, &mock);
            // The older audit did not exist when the newer lane bulk-superseded
            // processing rows. Recovery must not strand it in that queue forever.
            let winner = terminal_winner(&feature.store, &panel, option).await?;
            let mut audit = SelfRoleAudit {
                event_id: panel.id.clone(),
                event_order: Some("0001".into()),
                guild_id: GUILD.into(),
                member_id: USER.into(),
                panel_id: panel.id.clone(),
                source_id: MESSAGE.into(),
                option_key: Some("old".into()),
                role_id: Some(OLD_ROLE.into()),
                source: PanelMode::Select,
                operation: RoleOperation::Replace,
                outcome: SettledOutcome::Rejected,
                code: None,
                reason: None,
                effects: if dirty {
                    AuditEffects {
                        attempted_added_role_ids: vec![OLD_ROLE.into()],
                        unresolved_added_role_ids: if pending {
                            vec![OLD_ROLE.into()]
                        } else {
                            vec![]
                        },
                        ..Default::default()
                    }
                } else {
                    AuditEffects::default()
                },
                desired_role_ids: vec![OLD_ROLE.into()],
                pre_mutation_role_ids: vec![],
            };
            let former = feature.store.claim_audit(&audit).await?.unwrap();
            assert!(
                feature
                    .store
                    .checkpoint_exchange(
                        &former,
                        &audit.effects,
                        case == "compensating",
                        Some(pending),
                    )
                    .await?
            );
            clock.store(NOW + 500, Ordering::SeqCst);
            if dry_run {
                let service = SelfRoleService::new(
                    runtime(pool, &clock, &mock),
                    SelfRoleGates {
                        panels: vec![panel.clone()],
                        dry_run: true,
                    },
                    &[GUILD.to_owned()].into_iter().collect(),
                )
                .unwrap();
                assert_eq!(service.recover_once().await.unwrap(), 1);
                assert_eq!(service.recover_once().await.unwrap(), 0); // no terminal discovery/acquisition
            } else {
                let hint = feature
                    .recovery_candidates(&panel, 1)
                    .await
                    .unwrap()
                    .remove(0);
                let result = feature.recover(&hint, &panel).await;
                if pending {
                    assert!(matches!(result, Err(RuntimeError::PendingExchange)));
                } else if dirty {
                    assert!(matches!(result, Err(RuntimeError::Stale)));
                } else {
                    assert!(matches!(
                        result.unwrap(),
                        Admission::Rejected("superseded_by_later_event")
                    ));
                }
            }
            assert!(mock.requests().is_empty()); // no old intent/planning REST or reply
            audit.code = Some("superseded_by_later_event".into());
            audit.reason = Some("a later exclusive-panel event was accepted".into());
            assert_eq!(
                terminal_evidence(pool, &audit, pending, false).await?,
                audit.effects
            );
            let flags: (i32, bool, bool, bool) = sqlx::query_as(
                "SELECT claim_generation,compensating,processing_expires_at IS NULL,
                 repair_expires_at IS NULL FROM self_role_audit WHERE event_id=$1",
            )
            .bind(&audit.event_id)
            .fetch_one(pool)
            .await?;
            assert_eq!(flags, (2, case == "compensating", true, true));
            assert!(!feature.store.owns_claim(&former).await?);
            assert!(
                !feature
                    .store
                    .record_superseded_exchange(&former, &AuditEffects::default(), Some(false))
                    .await?
            );
            assert!(feature
                .recovery_candidates(&panel, 1)
                .await
                .unwrap()
                .is_empty());
            let hints = feature.terminal_candidates(&panel, 1).await.unwrap();
            assert_eq!(hints.len(), usize::from(dirty));
            assert_terminal_winner(pool, &panel, &winner, option).await?;
            if dirty && !dry_run {
                let result = feature.recover_terminal(&hints[0], &panel).await;
                if pending {
                    assert!(matches!(result, Err(RuntimeError::PendingExchange)));
                } else {
                    assert!(result.unwrap());
                }
                let effects = terminal_evidence(pool, &audit, pending, !pending).await?;
                assert_eq!(effects.attempted_added_role_ids, [OLD_ROLE]);
                assert_eq!(
                    effects.unresolved_added_role_ids.contains(&OLD_ROLE.into()),
                    pending
                );
                let requests = mock.requests();
                let mutations: Vec<_> = requests.iter().filter(|r| r.method != "GET").collect();
                assert_eq!(mutations.len(), usize::from(!pending));
                if !pending {
                    assert_eq!(mutations[0].method, "DELETE");
                    assert!(mutations[0].path.ends_with(OLD_ROLE));
                    assert_eq!(effects.compensated_removed_role_ids, [OLD_ROLE]);
                }
                assert!(mutations
                    .iter()
                    .all(|r| !r.path.ends_with(OTHER) && !r.path.ends_with(NEW_ROLE)));
                assert_terminal_winner(pool, &panel, &winner, option).await?;
                if pending {
                    clock.store(NOW + 1_000, Ordering::SeqCst);
                    assert_eq!(
                        feature.terminal_candidates(&panel, 1).await.unwrap().len(),
                        1
                    );
                }
            }
            mock.shutdown().await;
        }
    }
    Ok(())
}

// Restart fixtures retain the rejected event's real panel/member/source ids and
// initialized immutable intent. Only a new maintenance claim may repair it.
async fn terminal_seed(
    store: &SelfRoleStore,
    panel: &SelfRolePanel,
    id: &str,
    pending: bool,
) -> TestResult<SelfRoleAudit> {
    let mut audit = SelfRoleAudit {
        event_id: id.into(),
        event_order: Some("0001".into()),
        guild_id: GUILD.into(),
        panel_id: panel.id.clone(),
        member_id: USER.into(),
        source_id: MESSAGE.into(),
        option_key: Some("old".into()),
        role_id: Some(OLD_ROLE.into()),
        source: panel.mode,
        operation: RoleOperation::Replace,
        outcome: SettledOutcome::Switched,
        code: None,
        reason: None,
        effects: AuditEffects {
            attempted_added_role_ids: vec![OLD_ROLE.into()],
            unresolved_added_role_ids: if pending {
                vec![OLD_ROLE.into()]
            } else {
                vec![]
            },
            ..Default::default()
        },
        desired_role_ids: vec![OLD_ROLE.into()],
        pre_mutation_role_ids: vec![],
    };
    let event = store.claim_audit(&audit).await?.unwrap();
    assert!(event.intent_initialized);
    assert!(
        store
            .checkpoint_exchange(&event, &audit.effects, false, Some(pending))
            .await?
    );
    audit.outcome = SettledOutcome::Rejected;
    audit.code = Some("superseded_by_later_event".into());
    audit.reason = Some("a later exclusive-panel event was accepted".into());
    store.finish_audit(&audit, &event).await?;
    Ok(audit)
}

async fn terminal_winner(
    store: &SelfRoleStore,
    panel: &SelfRolePanel,
    option: Option<&str>,
) -> TestResult<String> {
    let winner = format!("{}-winner", panel.id);
    let key = PanelKey {
        guild_id: GUILD.into(),
        member_id: USER.into(),
        panel_id: panel.id.clone(),
    };
    let mut claim = match store.claim_panel(&key, Some((&winner, "0002"))).await? {
        PanelClaimResult::Acquired(claim) => claim,
        other => panic!("expected winner lane, got {other:?}"),
    };
    assert!(store.set_panel_claim_option(&mut claim, option).await?);
    assert!(store.release_panel_claim(&claim).await?);
    Ok(winner)
}

async fn assert_terminal_winner(
    pool: &PgPool,
    panel: &SelfRolePanel,
    winner: &str,
    option: Option<&str>,
) -> TestResult {
    let (event, order, selected, committed): (String, String, Option<String>, bool) =
        sqlx::query_as(
            "SELECT latest_event_id,latest_event_order,latest_option_key,target_committed
         FROM self_role_panel_claims WHERE guild_id=$1 AND member_id=$2 AND panel_id=$3",
        )
        .bind(GUILD)
        .bind(USER)
        .bind(&panel.id)
        .fetch_one(pool)
        .await?;
    assert_eq!(event, winner);
    assert_eq!(order, "0002");
    assert_eq!(selected.as_deref(), option);
    assert!(committed);
    Ok(())
}

async fn terminal_evidence(
    pool: &PgPool,
    audit: &SelfRoleAudit,
    pending: bool,
    complete: bool,
) -> TestResult<AuditEffects> {
    let (outcome, code, reason, order, desired, before, exchange, receipt, effects): (
        String,
        String,
        String,
        Option<String>,
        String,
        String,
        bool,
        bool,
        Vec<String>,
    ) = sqlx::query_as(
        "SELECT outcome,code,reason,event_order,desired_role_ids,pre_mutation_role_ids,
         exchange_pending,repair_complete,ARRAY[added_role_ids,removed_role_ids,
         attempted_added_role_ids,attempted_removed_role_ids,compensated_added_role_ids,
         compensated_removed_role_ids,unresolved_added_role_ids,unresolved_removed_role_ids]
         FROM self_role_audit WHERE event_id=$1",
    )
    .bind(&audit.event_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(outcome, "rejected");
    assert_eq!(Some(code.as_str()), audit.code.as_deref());
    assert_eq!(Some(reason.as_str()), audit.reason.as_deref());
    assert_eq!(order, audit.event_order);
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&desired)?,
        audit.desired_role_ids
    );
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&before)?,
        audit.pre_mutation_role_ids
    );
    assert_eq!(exchange, pending);
    assert_eq!(receipt, complete);
    Ok(AuditEffects {
        added_role_ids: serde_json::from_str(&effects[0])?,
        removed_role_ids: serde_json::from_str(&effects[1])?,
        attempted_added_role_ids: serde_json::from_str(&effects[2])?,
        attempted_removed_role_ids: serde_json::from_str(&effects[3])?,
        compensated_added_role_ids: serde_json::from_str(&effects[4])?,
        compensated_removed_role_ids: serde_json::from_str(&effects[5])?,
        unresolved_added_role_ids: serde_json::from_str(&effects[6])?,
        unresolved_removed_role_ids: serde_json::from_str(&effects[7])?,
    })
}

async fn terminal_restart_repairs_committed_target(pool: &PgPool) -> TestResult {
    for case in ["selected", "selected-missing", "empty"] {
        let clock = Arc::new(AtomicI64::new(NOW));
        let mut panel = panel(PanelMode::Select);
        panel.id = format!("terminal-restart-{case}");
        let option = if case == "empty" { None } else { Some("new") };
        let held = if case == "selected-missing" {
            vec![OLD_ROLE, OTHER]
        } else {
            vec![OLD_ROLE, NEW_ROLE, OTHER]
        };
        let mut script = snapshot(&held);
        script.push(ScriptedResponse::status(204)); // DELETE obsolete old first
        if case == "selected-missing" {
            script.extend(snapshot(&[OTHER]));
            script.push(ScriptedResponse::status(204)); // PUT missing committed new
        } else if case == "empty" {
            script.extend(snapshot(&[NEW_ROLE, OTHER]));
            script.push(ScriptedResponse::status(204)); // DELETE remaining panel role
        }
        script.extend(snapshot(if case == "empty" {
            &[OTHER]
        } else {
            &[NEW_ROLE, OTHER]
        }));
        let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
        let feature = runtime(pool, &clock, &mock);
        let audit = terminal_seed(&feature.store, &panel, &panel.id, false).await?;
        let winner = terminal_winner(&feature.store, &panel, option).await?;
        let hints = feature.terminal_candidates(&panel, 1).await.unwrap();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].event_id, audit.event_id);
        assert!(feature.recover_terminal(&hints[0], &panel).await.unwrap());
        assert!(!feature.recover_terminal(&hints[0], &panel).await.unwrap());
        assert!(feature
            .terminal_candidates(&panel, 1)
            .await
            .unwrap()
            .is_empty());
        let effects = terminal_evidence(pool, &audit, false, true).await?;
        assert!(effects.attempted_added_role_ids.contains(&OLD_ROLE.into()));
        assert!(effects
            .compensated_removed_role_ids
            .contains(&OLD_ROLE.into()));
        if case == "selected-missing" {
            assert!(effects.attempted_added_role_ids.contains(&NEW_ROLE.into()));
            assert_eq!(effects.compensated_added_role_ids, [NEW_ROLE]);
        } else if case == "empty" {
            assert!(effects
                .compensated_removed_role_ids
                .contains(&NEW_ROLE.into()));
        }
        assert!(effects.unresolved_added_role_ids.is_empty());
        assert!(effects.unresolved_removed_role_ids.is_empty());
        assert_terminal_winner(pool, &panel, &winner, option).await?;
        let calls = mock.requests();
        let mutations: Vec<_> = calls.iter().filter(|r| r.method != "GET").collect();
        let actual: Vec<_> = mutations
            .iter()
            .map(|r| (r.method.as_str(), r.path.as_str()))
            .collect();
        let old_path = format!("/api/v10/guilds/{GUILD}/members/{USER}/roles/{OLD_ROLE}");
        let new_path = format!("/api/v10/guilds/{GUILD}/members/{USER}/roles/{NEW_ROLE}");
        let mut expected = vec![("DELETE", old_path.as_str())];
        if case == "selected-missing" {
            expected.push(("PUT", new_path.as_str()));
        } else if case == "empty" {
            expected.push(("DELETE", new_path.as_str()));
        }
        assert_eq!(actual, expected);
        assert_eq!(calls.len(), if case == "selected" { 9 } else { 14 });
        assert!(mutations
            .iter()
            .all(|r| r.body.is_empty() && !r.path.ends_with(OTHER)));
        mock.shutdown().await;
    }
    Ok(())
}

async fn terminal_restart_refuses_unknown_or_uninitialized_target(pool: &PgPool) -> TestResult {
    for case in ["unknown", "uninitialized", "removed-option"] {
        let clock = Arc::new(AtomicI64::new(NOW));
        let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
        let feature = runtime(pool, &clock, &mock);
        let mut panel = panel(PanelMode::Select);
        panel.id = format!("terminal-refuse-{case}");
        let audit = terminal_seed(&feature.store, &panel, &panel.id, false).await?;
        let winner = if case == "unknown" {
            None // a maintenance lane without a committed target is not empty
        } else {
            Some(
                terminal_winner(
                    &feature.store,
                    &panel,
                    Some(if case == "removed-option" {
                        "no-longer-offered"
                    } else {
                        "new"
                    }),
                )
                .await?,
            )
        };
        if case == "uninitialized" {
            sqlx::query("UPDATE self_role_audit SET intent_initialized=FALSE WHERE event_id=$1")
                .bind(&audit.event_id)
                .execute(pool)
                .await?;
        }
        let hints = feature.terminal_candidates(&panel, 1).await.unwrap();
        assert_eq!(hints.len(), 1);
        assert!(matches!(
            feature.recover_terminal(&hints[0], &panel).await,
            Err(RuntimeError::InvalidSnapshot)
        ));
        assert!(mock.requests().is_empty()); // no reads or mutation before refusal
        terminal_evidence(pool, &audit, false, false).await?;
        if let Some(winner) = winner {
            assert_terminal_winner(
                pool,
                &panel,
                &winner,
                Some(if case == "removed-option" {
                    "no-longer-offered"
                } else {
                    "new"
                }),
            )
            .await?;
        } else {
            let (committed, option): (bool, Option<String>) = sqlx::query_as(
                "SELECT target_committed,latest_option_key FROM self_role_panel_claims
                 WHERE guild_id=$1 AND member_id=$2 AND panel_id=$3",
            )
            .bind(GUILD)
            .bind(USER)
            .bind(&panel.id)
            .fetch_one(pool)
            .await?;
            assert!(!committed);
            assert!(option.is_none());
        }
        mock.shutdown().await;
    }
    Ok(())
}

async fn terminal_restart_preserves_inherited_pending(pool: &PgPool) -> TestResult {
    for converged in [true, false] {
        let clock = Arc::new(AtomicI64::new(NOW));
        let mut panel = panel(PanelMode::Select);
        panel.id = format!("terminal-pending-{converged}");
        let mut script = if converged {
            snapshot(&[NEW_ROLE, OTHER])
        } else {
            snapshot(&[OLD_ROLE, OTHER])
        };
        if !converged {
            script.push(ScriptedResponse::status(204)); // fresh DELETE response
            script.extend(snapshot(&[OTHER]));
            script.push(ScriptedResponse::status(204)); // fresh PUT response
            script.extend(snapshot(&[NEW_ROLE, OTHER]));
        }
        let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
        let feature = runtime(pool, &clock, &mock);
        let audit = terminal_seed(&feature.store, &panel, &panel.id, true).await?;
        let winner = terminal_winner(&feature.store, &panel, Some("new")).await?;
        let hint = feature
            .terminal_candidates(&panel, 1)
            .await
            .unwrap()
            .remove(0);
        assert!(matches!(
            feature.recover_terminal(&hint, &panel).await,
            Err(RuntimeError::PendingExchange)
        ));
        let effects = terminal_evidence(pool, &audit, true, false).await?;
        // Fresh acknowledged DELETE/PUT responses are not unknown just because
        // the original process still has an unresolved addition of OLD_ROLE.
        assert_eq!(effects.unresolved_added_role_ids, [OLD_ROLE]);
        assert!(effects.unresolved_removed_role_ids.is_empty());
        assert!(effects.attempted_added_role_ids.contains(&OLD_ROLE.into()));
        if !converged {
            assert_eq!(effects.compensated_removed_role_ids, [OLD_ROLE]);
            assert_eq!(effects.compensated_added_role_ids, [NEW_ROLE]);
        }
        assert_terminal_winner(pool, &panel, &winner, Some("new")).await?;
        let calls = mock.requests();
        let methods: Vec<_> = calls
            .iter()
            .filter(|r| r.method != "GET")
            .map(|r| r.method.as_str())
            .collect();
        assert_eq!(
            methods,
            if converged {
                vec![]
            } else {
                vec!["DELETE", "PUT"]
            }
        );
        assert_eq!(calls.len(), if converged { 4 } else { 14 });
        // A response to this repair (or a converged member) never resolves the
        // prior process's unknown exchange. It remains due after lease expiry.
        clock.store(NOW + 500, Ordering::SeqCst);
        let retry = feature.terminal_candidates(&panel, 1).await.unwrap();
        assert_eq!(retry.len(), 1);
        assert_eq!(retry[0].event_id, audit.event_id);
        mock.shutdown().await;
    }
    Ok(())
}

async fn terminal_restart_cancellation_preserves_journal(pool: &PgPool) -> TestResult {
    let clock = Arc::new(AtomicI64::new(NOW));
    let mut panel = panel(PanelMode::Select);
    panel.id = "terminal-cancel-delayed-repair".into();
    let mut script = snapshot(&[OLD_ROLE, OTHER]);
    script.push(ScriptedResponse::status(204).delayed(Duration::from_secs(5)));
    let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
    let feature = Arc::new(runtime(pool, &clock, &mock));
    let audit = terminal_seed(&feature.store, &panel, &panel.id, false).await?;
    let winner = terminal_winner(&feature.store, &panel, Some("new")).await?;
    let hint = feature
        .terminal_candidates(&panel, 1)
        .await
        .unwrap()
        .remove(0);
    let owner = {
        let feature = feature.clone();
        let panel = panel.clone();
        tokio::spawn(async move { feature.recover_terminal(&hint, &panel).await })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while !mock.requests().iter().any(|r| r.method == "DELETE") {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    clock.store(NOW + 100, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(250)).await;
    let renewed: (i64, i64) = sqlx::query_as(
        "SELECT (extract(epoch FROM a.repair_expires_at)*1000)::bigint,
         (extract(epoch FROM p.processing_expires_at)*1000)::bigint
         FROM self_role_audit a JOIN self_role_panel_claims p USING(guild_id,member_id,panel_id)
         WHERE a.event_id=$1",
    )
    .bind(&audit.event_id)
    .fetch_one(pool)
    .await?;
    assert!(renewed.0 > NOW + 300 && renewed.1 > NOW + 300);
    assert!(!owner.is_finished());
    owner.abort();
    assert!(owner.await.unwrap_err().is_cancelled());
    let effects = terminal_evidence(pool, &audit, true, false).await?;
    assert_eq!(effects.attempted_added_role_ids, [OLD_ROLE]);
    assert_eq!(effects.attempted_removed_role_ids, [OLD_ROLE]);
    assert_eq!(effects.unresolved_removed_role_ids, [OLD_ROLE]);
    assert!(effects.compensated_removed_role_ids.is_empty());
    let receipts = role_receipts(pool, &audit.event_id).await?;
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].1, OLD_ROLE);
    assert!(!receipts[0].2 && receipts[0].3);
    assert_eq!(receipts[0].4, "pending");
    assert_eq!(receipts[0].5, None);
    assert_terminal_winner(pool, &panel, &winner, Some("new")).await?;
    let expiry: (i64, i64) = sqlx::query_as(
        "SELECT (extract(epoch FROM a.repair_expires_at)*1000)::bigint,
         (extract(epoch FROM p.processing_expires_at)*1000)::bigint
         FROM self_role_audit a JOIN self_role_panel_claims p USING(guild_id,member_id,panel_id)
         WHERE a.event_id=$1",
    )
    .bind(&audit.event_id)
    .fetch_one(pool)
    .await?;
    let calls = mock.requests();
    clock.store(NOW + 200, Ordering::SeqCst); // both old leases are still live
    tokio::time::sleep(Duration::from_millis(250)).await;
    let after: (i64, i64) = sqlx::query_as(
        "SELECT (extract(epoch FROM a.repair_expires_at)*1000)::bigint,
         (extract(epoch FROM p.processing_expires_at)*1000)::bigint
         FROM self_role_audit a JOIN self_role_panel_claims p USING(guild_id,member_id,panel_id)
         WHERE a.event_id=$1",
    )
    .bind(&audit.event_id)
    .fetch_one(pool)
    .await?;
    assert_eq!(expiry, after); // neither renewal keeper escaped its cancelled owner
    assert_eq!(calls.len(), 5);
    assert_eq!(calls.len(), mock.requests().len());
    assert_eq!(calls[4].method, "DELETE");
    assert!(calls[4].path.ends_with(OLD_ROLE));
    let after_effects = terminal_evidence(pool, &audit, true, false).await?;
    assert_eq!(
        effects.attempted_removed_role_ids,
        after_effects.attempted_removed_role_ids
    );
    assert_eq!(
        effects.unresolved_removed_role_ids,
        after_effects.unresolved_removed_role_ids
    );
    clock.store(NOW + 1_000, Ordering::SeqCst);
    assert_eq!(
        feature.terminal_candidates(&panel, 1).await.unwrap().len(),
        1
    );
    assert_eq!(role_receipts(pool, &audit.event_id).await?, receipts);
    mock.shutdown().await;
    Ok(())
}

async fn terminal_partial_repair_exchange_outcomes(pool: &PgPool) -> TestResult {
    for case in [
        "forbidden",
        "rate-limited",
        "server-error",
        "timeout",
        "truncated-forbidden",
        "stalled-forbidden",
        "truncated-rate-limited",
        "stalled-rate-limited",
        "truncated-server-error",
        "stalled-server-error",
    ] {
        let clock = Arc::new(AtomicI64::new(NOW));
        let mut panel = panel(PanelMode::Select);
        panel.id = format!("terminal-partial-{case}");
        let pending = case == "timeout";
        let outcome = case
            .strip_prefix("truncated-")
            .or_else(|| case.strip_prefix("stalled-"))
            .unwrap_or(case);
        let failure = match outcome {
            "forbidden" => ScriptedResponse::json(403, json!({"message":"provider-secret"})),
            "rate-limited" => ScriptedResponse::rate_limited(0.1, "0.1"),
            "server-error" => ScriptedResponse::json(500, json!({"message":"provider-secret"})),
            "timeout" => ScriptedResponse::status(204).delayed(Duration::from_secs(6)),
            _ => unreachable!(),
        };
        let failure = if case.starts_with("truncated-") {
            failure.truncated_body()
        } else if case.starts_with("stalled-") {
            failure.delayed_body(Duration::from_secs(6))
        } else {
            failure
        };
        let expected = match outcome {
            "forbidden" => SelfRoleRestError::Rejected(403),
            "rate-limited" => SelfRoleRestError::RateLimited,
            _ => SelfRoleRestError::Ambiguous,
        };
        let mut script = snapshot(&[OLD_ROLE, OTHER]);
        script.push(ScriptedResponse::status(204)); // acknowledged obsolete-role removal
        script.extend(snapshot(&[OTHER]));
        script.push(failure); // only one add attempt, no executor retry
        script.extend(snapshot(&[NEW_ROLE, OTHER])); // later authoritative convergence
        let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
        let feature = runtime(pool, &clock, &mock);
        let audit = terminal_seed(&feature.store, &panel, &panel.id, false).await?;
        let winner = terminal_winner(&feature.store, &panel, Some("new")).await?;
        let hint = feature
            .terminal_candidates(&panel, 1)
            .await
            .unwrap()
            .remove(0);
        assert!(matches!(feature.recover_terminal(&hint, &panel).await,
            Err(RuntimeError::Rest(error)) if error == expected));
        let effects = terminal_evidence(pool, &audit, pending, false).await?;
        assert_eq!(effects.compensated_removed_role_ids, [OLD_ROLE]);
        assert!(effects.compensated_added_role_ids.is_empty());
        assert_eq!(effects.attempted_removed_role_ids, [OLD_ROLE]);
        assert!(effects.attempted_added_role_ids.contains(&NEW_ROLE.into()));
        assert_eq!(
            effects.unresolved_added_role_ids.contains(&NEW_ROLE.into()),
            matches!(outcome, "server-error" | "timeout")
        );
        assert_terminal_winner(pool, &panel, &winner, Some("new")).await?;
        let receipts = role_receipts(pool, &audit.event_id).await?;
        let status = match outcome {
            "forbidden" => Some(403),
            "rate-limited" => Some(429),
            "server-error" => Some(500),
            "timeout" => None,
            _ => unreachable!(),
        };
        assert_eq!(
            receipts
                .iter()
                .map(|r| (r.1.as_str(), r.2, r.3, r.4.as_str(), r.5))
                .collect::<Vec<_>>(),
            [
                (OLD_ROLE, false, true, "response", Some(204)),
                (
                    NEW_ROLE,
                    true,
                    true,
                    if pending { "pending" } else { "response" },
                    status
                ),
            ]
        );
        let calls = mock.requests();
        let mutations: Vec<_> = calls.iter().filter(|r| r.method != "GET").collect();
        assert_eq!(calls.len(), 10);
        assert_eq!(mutations.len(), 2);
        assert_eq!(mutations[0].method, "DELETE");
        assert!(mutations[0].path.ends_with(OLD_ROLE));
        assert_eq!(mutations[1].method, "PUT");
        assert!(mutations[1].path.ends_with(NEW_ROLE));
        assert!(mutations
            .iter()
            .all(|r| r.body.is_empty() && !r.path.ends_with(OTHER)));

        clock.store(NOW + 1_000, Ordering::SeqCst);
        let hint = feature
            .terminal_candidates(&panel, 1)
            .await
            .unwrap()
            .remove(0);
        let repaired = feature.recover_terminal(&hint, &panel).await;
        if pending {
            assert!(matches!(repaired, Err(RuntimeError::PendingExchange)));
        } else {
            assert!(repaired.unwrap());
        }
        let after = terminal_evidence(pool, &audit, pending, !pending).await?;
        assert_eq!(role_receipts(pool, &audit.event_id).await?, receipts);
        assert_eq!(after.compensated_removed_role_ids, [OLD_ROLE]);
        // A snapshot observes the attempted addition; it does not fabricate an
        // acknowledged compensation response to the prior failed/unknown PUT.
        assert_eq!(after.added_role_ids, [NEW_ROLE]);
        assert!(after.compensated_added_role_ids.is_empty());
        assert_eq!(
            after.unresolved_added_role_ids.contains(&NEW_ROLE.into()),
            pending
        );
        assert_eq!(mock.requests().len(), 14);
        assert_eq!(
            mock.requests().iter().filter(|r| r.method != "GET").count(),
            2
        );
        assert_terminal_winner(pool, &panel, &winner, Some("new")).await?;
        // Even after the delayed mock send has finished, its lost response was
        // not received by this owner. Elapsed time cannot retire that uncertainty.
        if pending {
            tokio::time::sleep(Duration::from_millis(1_100)).await;
            terminal_evidence(pool, &audit, true, false).await?;
        }
        mock.shutdown().await;
    }
    Ok(())
}

async fn terminal_fixture_owners(
    feature: &SelfRoleRuntime,
    panel: &SelfRolePanel,
) -> TestResult<(TerminalRepair, RepairLease)> {
    let hint = feature
        .terminal_candidates(panel, 1)
        .await
        .unwrap()
        .remove(0);
    let claim = feature.store.claim_superseded_audit(&hint).await?.unwrap();
    let lost = Arc::new(AtomicBool::new(false));
    let owner = TerminalRepair {
        effects: claim.audit().effects.clone(),
        pending: claim.exchange_pending(),
        _keeper: LeaseKeeper::terminal(feature.store.clone(), claim.clone(), lost.clone()),
        claim,
        lost: lost.clone(),
    };
    let key = PanelKey {
        guild_id: GUILD.into(),
        member_id: USER.into(),
        panel_id: panel.id.clone(),
    };
    let claim = match feature.store.claim_panel(&key, None).await? {
        PanelClaimResult::Acquired(claim) => claim,
        other => panic!("expected maintenance lane, got {other:?}"),
    };
    let lane = RepairLease {
        _keeper: LeaseKeeper::start(
            feature.store.clone(),
            None,
            Some(claim.clone()),
            lost.clone(),
        ),
        claim,
        lost,
    };
    // These fault fixtures advance the DB clock independently for each lease.
    // Disable only their owned keepers; no host process or cache is touched.
    owner._keeper.0.abort();
    lane._keeper.0.abort();
    Ok((owner, lane))
}

async fn exchange_owner_state(pool: &PgPool, event: &str) -> TestResult<(String, Option<String>)> {
    Ok(sqlx::query_as(
        "SELECT (to_jsonb(a)-'claim_token')::text,(to_jsonb(p)-'claim_token')::text
         FROM self_role_audit a LEFT JOIN self_role_panel_claims p
         USING(guild_id,member_id,panel_id) WHERE a.event_id=$1",
    )
    .bind(event)
    .fetch_one(pool)
    .await?)
}

type RoleReceipt = (i32, String, bool, bool, String, Option<i16>);

async fn role_receipts(pool: &PgPool, event: &str) -> TestResult<Vec<RoleReceipt>> {
    Ok(sqlx::query_as(
        "SELECT origin_generation,role_id,adding,compensating,disposition,response_status
         FROM self_role_exchanges WHERE event_id=$1 ORDER BY created_at,exchange_id",
    )
    .bind(event)
    .fetch_all(pool)
    .await?)
}

async fn processing_receipts_survive_generation_transfer(pool: &PgPool) -> TestResult {
    for exclusive in [false, true] {
        for status in [204, 403, 429, 500] {
            let clock = Arc::new(AtomicI64::new(NOW));
            let mut panel = panel(PanelMode::Select);
            panel.id = format!("processing-receipt-{exclusive}-{status}");
            panel.exclusive = exclusive;
            let mut script = snapshot(&[OLD_ROLE, OTHER]);
            script.push(ScriptedResponse::status(status).delayed(Duration::from_secs(3)));
            script.extend(snapshot(&[OLD_ROLE, OTHER]));
            let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
            let feature = runtime(pool, &clock, &mock);
            let request = request(
                &panel.id,
                Selection::Select {
                    option_keys: vec!["new".into()],
                },
            );
            let mut prepared = ready(feature.prepare(&request, &panel).await.unwrap());
            prepared._event_keeper.0.abort();
            prepared._panel_keeper.take();
            let original = prepared.event.clone();
            let audit = prepared.audit.clone();
            let panel_claim = prepared.panel.clone();
            let mut step = Box::pin(feature.execute_step(&mut prepared, OLD_ROLE, false, None));
            tokio::select! {
                result = step.as_mut() => panic!("step completed before transfer: {result:?}"),
                result = tokio::time::timeout(Duration::from_secs(2), async {
                    while !mock.requests().iter().any(|r| r.method == "DELETE") {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }) => result?,
            }
            clock.store(NOW + 100, Ordering::SeqCst);
            if let Some(panel) = &panel_claim {
                assert!(feature.store.renew_panel_claim(panel).await?);
            }
            clock.store(NOW + 300, Ordering::SeqCst);
            let replacement = feature.store.claim_audit(&audit).await?.unwrap();
            assert_eq!(replacement.generation, original.generation + 1);
            assert!(replacement.exchange_pending);
            let owner_state = exchange_owner_state(pool, &request.event_id).await?;
            assert!(matches!(step.await, Err(RuntimeError::Stale)));
            // The late response updates ONLY its receipt, not the replacement's
            // immutable intent, effect checkpoint, phase, claims or target.
            assert_eq!(
                exchange_owner_state(pool, &request.event_id).await?,
                owner_state
            );
            assert_eq!(
                role_receipts(pool, &request.event_id).await?,
                [(
                    original.generation,
                    OLD_ROLE.into(),
                    false,
                    false,
                    "response".into(),
                    Some(status as i16)
                )]
            );
            assert!(
                !feature
                    .store
                    .checkpoint_exchange(&original, &prepared.audit.effects, false, Some(false),)
                    .await?
            );
            assert!(
                feature
                    .store
                    .checkpoint_exchange(
                        &replacement,
                        &replacement.effects,
                        replacement.compensating,
                        Some(replacement.exchange_pending),
                    )
                    .await?
            );
            let pending: bool = sqlx::query_scalar(
                "SELECT exchange_pending FROM self_role_audit WHERE event_id=$1",
            )
            .bind(&request.event_id)
            .fetch_one(pool)
            .await?;
            assert!(pending); // A store-only checkpoint does not retire receipts.
            feature.park(&mut prepared).await;
            drop(prepared);
            clock.store(NOW + 601, Ordering::SeqCst);
            let receipts = role_receipts(pool, &request.event_id).await?;
            let mut current = ready(feature.prepare(&request, &panel).await.unwrap());
            assert!(current.event.recovered);
            assert!(!current.event.exchange_pending);
            assert_eq!(current.event.desired_role_ids, [NEW_ROLE]);
            assert_eq!(current.event.pre_mutation_role_ids, [OLD_ROLE]);
            assert_eq!(current.audit.effects.attempted_removed_role_ids, [OLD_ROLE]);
            // Recovered prepare retires completed send provenance under its new
            // live fences. A received 5xx still has effect uncertainty until a
            // subsequent observation, but is not an unknown in-flight exchange.
            assert_eq!(
                current.audit.effects.unresolved_removed_role_ids,
                if status == 500 {
                    vec![OLD_ROLE.to_owned()]
                } else {
                    vec![]
                }
            );
            assert!(current
                .audit
                .effects
                .compensated_removed_role_ids
                .is_empty());
            assert_eq!(role_receipts(pool, &request.event_id).await?, receipts);
            let held = current.snapshot.member_role_ids.clone();
            observe_prepared(&mut current, &held);
            feature.checkpoint(&mut current).await.unwrap();
            feature.checkpoint(&mut current).await.unwrap();
            assert!(!current.event.exchange_pending);
            assert!(current.audit.effects.unresolved_removed_role_ids.is_empty());
            assert_eq!(current.audit.effects.attempted_removed_role_ids, [OLD_ROLE]);
            assert_eq!(role_receipts(pool, &request.event_id).await?, receipts);
            assert_eq!(mock.requests().len(), 9);
            assert_eq!(
                mock.requests().iter().filter(|r| r.method != "GET").count(),
                1
            );
            feature.park(&mut current).await;
            drop(current);
            mock.shutdown().await;
        }
    }
    Ok(())
}

async fn repeated_processing_tickets_preserve_pending(pool: &PgPool) -> TestResult {
    for exclusive in [false, true] {
        for status in [204, 403, 429, 500] {
            let clock = Arc::new(AtomicI64::new(NOW));
            let mut panel = panel(PanelMode::Select);
            panel.id = format!("processing-repeated-ticket-{exclusive}-{status}");
            panel.exclusive = exclusive;
            let mut script = snapshot(&[OLD_ROLE, OTHER]);
            script.push(ScriptedResponse::status(status));
            script.extend(snapshot(&[OLD_ROLE, OTHER])); // recovered prepare
            script.extend(snapshot(&[OLD_ROLE, OTHER])); // observation after retirement
            let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
            let feature = runtime(pool, &clock, &mock);
            let request = request(
                &panel.id,
                Selection::Select {
                    option_keys: vec!["new".into()],
                },
            );
            let mut prepared = ready(feature.prepare(&request, &panel).await.unwrap());
            let original = prepared.event.clone();
            let original_lane = prepared.panel.clone();
            // Hold one sender-owned ticket at the shared journal seam without
            // dispatching it. The runtime's next same-role/direction send gets
            // its own ticket and a real response from the existing REST double.
            let pending_ticket = feature
                .store
                .journal_role_exchange(
                    &prepared.event,
                    prepared.panel.as_ref(),
                    &ExchangeIntent {
                        role_id: NEW_ROLE.into(),
                        adding: true,
                        compensating: false,
                    },
                )
                .await?
                .unwrap();
            feature.checkpoint(&mut prepared).await.unwrap();
            assert!(prepared.event.exchange_pending);
            feature
                .execute_step(&mut prepared, NEW_ROLE, true, None)
                .await
                .unwrap();
            assert!(prepared.event.exchange_pending);
            assert_eq!(prepared.audit.effects.attempted_added_role_ids, [NEW_ROLE]);
            assert_eq!(prepared.audit.effects.unresolved_added_role_ids, [NEW_ROLE]);
            let receipts = role_receipts(pool, &request.event_id).await?;
            assert_eq!(
                receipts,
                [
                    (
                        original.generation,
                        NEW_ROLE.into(),
                        true,
                        false,
                        "pending".into(),
                        None,
                    ),
                    (
                        original.generation,
                        NEW_ROLE.into(),
                        true,
                        false,
                        "response".into(),
                        Some(status as i16),
                    ),
                ]
            );
            let retired: (i64, i64) = sqlx::query_as(
                "SELECT count(*) FILTER (WHERE retired_at IS NOT NULL),
                 count(*) FILTER (WHERE disposition='pending' AND retired_at IS NULL)
                 FROM self_role_exchanges WHERE event_id=$1",
            )
            .bind(&request.event_id)
            .fetch_one(pool)
            .await?;
            assert_eq!(retired, (1, 1));
            feature.park(&mut prepared).await;
            drop(prepared);
            clock.store(NOW + 301, Ordering::SeqCst);
            let mut current = ready(feature.prepare(&request, &panel).await.unwrap());
            assert!(current.event.recovered);
            assert_eq!(current.event.generation, original.generation + 1);
            assert_eq!(current.event.desired_role_ids, [NEW_ROLE]);
            assert_eq!(current.event.pre_mutation_role_ids, [OLD_ROLE]);
            assert_eq!(current.event.compensating, status != 204);
            assert!(current.event.exchange_pending);
            assert_eq!(current.audit.effects.unresolved_added_role_ids, [NEW_ROLE]);
            let held = current.snapshot.member_role_ids.clone();
            observe_prepared(&mut current, &held);
            feature.checkpoint(&mut current).await.unwrap();
            assert!(current.event.exchange_pending);
            assert_eq!(current.audit.effects.unresolved_added_role_ids, [NEW_ROLE]);
            assert_eq!(role_receipts(pool, &request.event_id).await?, receipts);
            let owner_state = exchange_owner_state(pool, &request.event_id).await?;
            assert!(feature
                .store
                .retire_role_receipts(&original, original_lane.as_ref())
                .await?
                .is_none());
            assert_eq!(
                exchange_owner_state(pool, &request.event_id).await?,
                owner_state
            );
            // Only the fixture's undispatched ticket has definite no-send
            // provenance. Completing it cannot checkpoint the replacement owner.
            assert!(
                feature
                    .store
                    .complete_role_exchange(&pending_ticket, ExchangeReceipt::NoSend)
                    .await?
            );
            assert!(
                !feature
                    .store
                    .complete_role_exchange(
                        &pending_ticket,
                        ExchangeReceipt::Response { status: 204 }
                    )
                    .await?
            );
            assert_eq!(
                exchange_owner_state(pool, &request.event_id).await?,
                owner_state
            );
            let completed = role_receipts(pool, &request.event_id).await?;
            assert_eq!(completed[0].4, "no_send");
            assert_eq!(completed[0].5, None);
            assert_eq!(completed[1], receipts[1]);
            feature.checkpoint(&mut current).await.unwrap();
            assert!(!current.event.exchange_pending);
            assert_eq!(
                current.audit.effects.unresolved_added_role_ids,
                if status == 500 {
                    vec![NEW_ROLE.to_owned()]
                } else {
                    vec![]
                }
            );
            // A subsequent fresh snapshot may resolve already-retired 5xx
            // ambiguity. Repeated checkpoints must not resurrect that evidence.
            let observed = feature
                .executor
                .fetch_self_role_snapshot(GUILD, USER, BOT)
                .await
                .unwrap();
            observe_prepared(&mut current, &observed.member_role_ids);
            feature.checkpoint(&mut current).await.unwrap();
            feature.checkpoint(&mut current).await.unwrap();
            assert!(!current.event.exchange_pending);
            assert!(current.audit.effects.unresolved_added_role_ids.is_empty());
            assert_eq!(current.audit.effects.attempted_added_role_ids, [NEW_ROLE]);
            assert!(current.audit.effects.compensated_added_role_ids.is_empty());
            assert_eq!(role_receipts(pool, &request.event_id).await?, completed);
            assert!(feature.store.owns_claim(&current.event).await?);
            if let Some(lane) = &current.panel {
                assert!(feature.store.owns_panel_claim(lane).await?);
                assert_eq!(lane.target.option_key.as_deref(), Some("old"));
                assert!(lane.target.committed);
            }
            assert_eq!(mock.requests().len(), 13);
            let mutations: Vec<_> = mock
                .requests()
                .into_iter()
                .filter(|r| r.method != "GET")
                .collect();
            assert_eq!(mutations.len(), 1);
            assert_eq!(mutations[0].method, "PUT");
            assert!(mutations[0].path.ends_with(NEW_ROLE));
            assert!(mutations[0].body.is_empty());
            feature.park(&mut current).await;
            drop(current);
            mock.shutdown().await;
        }
    }
    Ok(())
}

async fn terminal_late_response_after_independent_transfer(pool: &PgPool) -> TestResult {
    for (status, evidence_transfer) in [
        (204, true),
        (204, false),
        (403, true),
        (429, true),
        (500, true),
    ] {
        let clock = Arc::new(AtomicI64::new(NOW));
        let mut panel = panel(PanelMode::Select);
        panel.id = format!("terminal-late-transfer-{status}-{evidence_transfer}");
        let mut script = snapshot(&[OLD_ROLE, OTHER]);
        script.push(ScriptedResponse::status(status).delayed(Duration::from_secs(3)));
        let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
        let feature = runtime(pool, &clock, &mock);
        let audit = terminal_seed(&feature.store, &panel, &panel.id, false).await?;
        let winner = terminal_winner(&feature.store, &panel, Some("new")).await?;
        let (mut owner, lane) = terminal_fixture_owners(&feature, &panel).await?;
        let mut snapshot = feature
            .executor
            .fetch_self_role_snapshot(GUILD, USER, BOT)
            .await
            .unwrap();
        let evidence_claim = owner.claim.clone();
        let mut step =
            Box::pin(feature.terminal_step(&mut owner, &lane, &mut snapshot, OLD_ROLE, false));
        tokio::select! {
            result = step.as_mut() => panic!("step completed before transfer: {result:?}"),
            result = tokio::time::timeout(Duration::from_secs(2), async {
                while !mock.requests().iter().any(|r| r.method == "DELETE") {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }) => result?,
        }
        clock.store(NOW + 100, Ordering::SeqCst);
        if evidence_transfer {
            assert!(feature.store.renew_panel_claim(&lane.claim).await?);
        } else {
            assert!(
                feature
                    .store
                    .renew_superseded_claim(&evidence_claim)
                    .await?
            );
        }
        clock.store(NOW + 300, Ordering::SeqCst);
        let replacement_evidence;
        let replacement_lane;
        if evidence_transfer {
            let hint = feature
                .terminal_candidates(&panel, 1)
                .await
                .unwrap()
                .remove(0);
            replacement_evidence =
                Some(feature.store.claim_superseded_audit(&hint).await?.unwrap());
            replacement_lane = None;
            assert!(feature.store.owns_panel_claim(&lane.claim).await?);
        } else {
            replacement_evidence = None;
            replacement_lane = Some(
                match feature.store.claim_panel(&lane.claim.key, None).await? {
                    PanelClaimResult::Acquired(claim) => claim,
                    other => panic!("expected replacement maintenance lane, got {other:?}"),
                },
            );
            assert!(feature.store.owns_superseded_claim(&evidence_claim).await?);
        }
        let owner_state = exchange_owner_state(pool, &audit.event_id).await?;
        assert!(matches!(step.await, Err(RuntimeError::Stale)));
        if evidence_transfer {
            assert_eq!(
                exchange_owner_state(pool, &audit.event_id).await?,
                owner_state
            );
        }
        assert_eq!(
            owner.effects.compensated_removed_role_ids,
            if status == 204 {
                vec![OLD_ROLE.to_owned()]
            } else {
                vec![]
            }
        );
        assert_eq!(
            role_receipts(pool, &audit.event_id).await?,
            [(
                evidence_claim.generation(),
                OLD_ROLE.into(),
                false,
                true,
                "response".into(),
                Some(status as i16)
            )]
        );
        let receipts = role_receipts(pool, &audit.event_id).await?;
        // Losing either fence prevents this sender from retiring its completed
        // ticket. Late compensation facts do not clear durable send provenance.
        let effects = terminal_evidence(pool, &audit, true, false).await?;
        assert_eq!(effects.attempted_removed_role_ids, [OLD_ROLE]);
        assert_eq!(effects.unresolved_removed_role_ids, [OLD_ROLE]);
        let retained = exchange_owner_state(pool, &audit.event_id).await?;
        assert!(feature
            .store
            .retire_terminal_receipts(&mut owner.claim, &lane.claim)
            .await?
            .is_none());
        assert_eq!(exchange_owner_state(pool, &audit.event_id).await?, retained);
        assert_eq!(role_receipts(pool, &audit.event_id).await?, receipts);
        if let Some(mut replacement) = replacement_evidence {
            assert_eq!(replacement.generation(), owner.claim.generation() + 1);
            assert!(replacement.exchange_pending());
            assert!(effects.compensated_removed_role_ids.is_empty());
            assert_eq!(effects.unresolved_removed_role_ids, [OLD_ROLE]);
            // The former response is receipt-only after transfer. The new owner
            // retains its saved aggregate pending/evidence until fenced receipt
            // incorporation, not an optimistic snapshot or timer.
            assert!(
                !feature
                    .store
                    .record_superseded_repair(&owner.claim, &owner.effects, false)
                    .await?
            );
            let incorporated = feature
                .store
                .incorporate_terminal_receipts(&replacement, &lane.claim)
                .await?
                .unwrap();
            assert!(incorporated.exchange_pending);
            assert_eq!(incorporated.effects.unresolved_removed_role_ids, [OLD_ROLE]);
            assert_eq!(
                incorporated.effects.compensated_removed_role_ids,
                if status == 204 {
                    vec![OLD_ROLE.to_owned()]
                } else {
                    vec![]
                }
            );
            assert_eq!(
                feature
                    .store
                    .incorporate_terminal_receipts(&replacement, &lane.claim)
                    .await?
                    .unwrap(),
                incorporated
            );
            assert!(
                feature
                    .store
                    .record_superseded_repair(
                        &replacement,
                        &incorporated.effects,
                        incorporated.exchange_pending,
                    )
                    .await?
            );
            let effects = terminal_evidence(pool, &audit, true, false).await?;
            assert_eq!(effects.unresolved_removed_role_ids, [OLD_ROLE]);
            assert!(
                !feature
                    .store
                    .finish_superseded_repair(&owner.claim, &lane.claim)
                    .await?
            );
            let retired = feature
                .store
                .retire_terminal_receipts(&mut replacement, &lane.claim)
                .await?
                .unwrap();
            assert!(!retired.exchange_pending);
            assert_eq!(
                retired.effects.unresolved_removed_role_ids,
                if status == 500 {
                    vec![OLD_ROLE.to_owned()]
                } else {
                    vec![]
                }
            );
            assert_eq!(
                retired.effects.compensated_removed_role_ids,
                incorporated.effects.compensated_removed_role_ids
            );
            assert_eq!(
                feature
                    .store
                    .retire_terminal_receipts(&mut replacement, &lane.claim)
                    .await?
                    .unwrap(),
                retired
            );
            assert_eq!(
                terminal_evidence(pool, &audit, false, false).await?,
                retired.effects
            );
        }
        if let Some(replacement) = replacement_lane {
            assert_eq!(replacement.generation, lane.claim.generation + 1);
            assert_eq!(effects.compensated_removed_role_ids, [OLD_ROLE]);
            assert_eq!(effects.unresolved_removed_role_ids, [OLD_ROLE]);
            assert!(!feature.store.release_panel_claim(&lane.claim).await?);
            assert!(feature.store.owns_panel_claim(&replacement).await?);
            assert!(
                !feature
                    .store
                    .finish_superseded_repair(&owner.claim, &lane.claim)
                    .await?
            );
            // The surviving evidence fence expires at +400; the replacement
            // lane remains live until +600. A newly acquired evidence owner can
            // now retire the response under both current fences, without a send.
            clock.store(NOW + 400, Ordering::SeqCst);
            let hint = feature
                .terminal_candidates(&panel, 1)
                .await
                .unwrap()
                .remove(0);
            let mut replacement_owner = feature.store.claim_superseded_audit(&hint).await?.unwrap();
            assert_eq!(replacement_owner.generation(), owner.claim.generation() + 1);
            assert!(replacement_owner.exchange_pending());
            assert!(feature.store.owns_panel_claim(&replacement).await?);
            let retired = feature
                .store
                .retire_terminal_receipts(&mut replacement_owner, &replacement)
                .await?
                .unwrap();
            assert!(!retired.exchange_pending);
            assert!(retired.effects.unresolved_removed_role_ids.is_empty());
            assert_eq!(retired.effects.compensated_removed_role_ids, [OLD_ROLE]);
            assert_eq!(
                terminal_evidence(pool, &audit, false, false).await?,
                retired.effects
            );
            assert!(feature.store.release_panel_claim(&replacement).await?);
        }
        assert_eq!(role_receipts(pool, &audit.event_id).await?, receipts);
        assert_terminal_winner(pool, &panel, &winner, Some("new")).await?;
        assert_eq!(mock.requests().len(), 5);
        assert_eq!(
            mock.requests().iter().filter(|r| r.method != "GET").count(),
            1
        );
        mock.shutdown().await;
    }
    Ok(())
}

async fn terminal_pacing_loss_is_definite_no_send(pool: &PgPool) -> TestResult {
    for inherited_pending in [false, true] {
        let clock = Arc::new(AtomicI64::new(NOW));
        let mut panel = panel(PanelMode::Select);
        panel.id = format!("terminal-pacing-loss-{inherited_pending}");
        let mut script = snapshot(&[OLD_ROLE, OTHER]);
        let mut blocker_reads = snapshot(&[OLD_ROLE, OTHER]);
        blocker_reads[0] =
            ScriptedResponse::json(200, json!({"user":{"id":USER},"roles":[OLD_ROLE,OTHER]}))
                .delayed(Duration::from_secs(1));
        script.extend(blocker_reads);
        let mock = MockRest::start(script, ScriptedResponse::status(500)).await;
        let feature = runtime(pool, &clock, &mock);
        let audit = terminal_seed(&feature.store, &panel, &panel.id, inherited_pending).await?;
        let winner = terminal_winner(&feature.store, &panel, Some("new")).await?;
        let (mut owner, lane) = terminal_fixture_owners(&feature, &panel).await?;
        let mut snapshot = feature
            .executor
            .fetch_self_role_snapshot(GUILD, USER, BOT)
            .await
            .unwrap();
        let blocker = {
            let executor = feature.executor.clone();
            tokio::spawn(async move { executor.fetch_self_role_snapshot(GUILD, USER, BOT).await })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while mock.requests().len() < 5 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let mut step =
            Box::pin(feature.terminal_step(&mut owner, &lane, &mut snapshot, OLD_ROLE, false));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), step.as_mut())
                .await
                .is_err()
        );
        clock.store(NOW + 300, Ordering::SeqCst); // expire while shared paced lane is occupied
        assert!(matches!(step.await, Err(RuntimeError::Stale)));
        blocker.await?.unwrap();
        let effects = terminal_evidence(pool, &audit, inherited_pending, false).await?;
        assert_eq!(effects.attempted_added_role_ids, [OLD_ROLE]);
        assert!(effects.attempted_removed_role_ids.is_empty()); // no journal, no send intent
        assert!(role_receipts(pool, &audit.event_id).await?.is_empty());
        assert!(effects.compensated_removed_role_ids.is_empty());
        assert!(effects.unresolved_removed_role_ids.is_empty());
        assert_eq!(
            effects.unresolved_added_role_ids.contains(&OLD_ROLE.into()),
            inherited_pending
        );
        assert_terminal_winner(pool, &panel, &winner, Some("new")).await?;
        assert_eq!(mock.requests().len(), 8);
        assert!(mock.requests().iter().all(|r| r.method == "GET"));
        mock.shutdown().await;
    }
    Ok(())
}

async fn terminal_post_journal_wait_is_definite_no_send(pool: &PgPool) -> TestResult {
    for inherited_pending in [false, true] {
        let clock = Arc::new(AtomicI64::new(NOW));
        let mut panel = panel(PanelMode::Select);
        panel.id = format!("terminal-journal-wait-{inherited_pending}");
        let mock =
            MockRest::start(snapshot(&[OLD_ROLE, OTHER]), ScriptedResponse::status(500)).await;
        let feature = runtime(pool, &clock, &mock);
        let audit = terminal_seed(&feature.store, &panel, &panel.id, inherited_pending).await?;
        let winner = terminal_winner(&feature.store, &panel, Some("new")).await?;
        let (mut owner, lane) = terminal_fixture_owners(&feature, &panel).await?;
        let mut snapshot = feature
            .executor
            .fetch_self_role_snapshot(GUILD, USER, BOT)
            .await
            .unwrap();

        // The generated test schema owns this trigger. Delay only the journal
        // UPDATE, after its pre-write ownership checks; read-only checks cannot
        // block here. This exercises the executor's post-journal fence without
        // adding a production hook or inventing a remote response.
        sqlx::raw_sql(
            "CREATE FUNCTION terminal_fixture_journal_wait() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
                 IF NEW.attempted_removed_role_ids IS DISTINCT FROM OLD.attempted_removed_role_ids THEN
                     PERFORM pg_advisory_xact_lock(hashtextextended(current_schema() || NEW.event_id, 0));
                 END IF;
                 RETURN NEW;
             END $$;
             CREATE TRIGGER terminal_fixture_journal_wait BEFORE UPDATE ON self_role_audit
             FOR EACH ROW EXECUTE FUNCTION terminal_fixture_journal_wait();",
        ).execute(pool).await?;
        let mut blocker = pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(current_schema() || $1, 0))")
            .bind(&audit.event_id)
            .execute(&mut *blocker)
            .await?;
        let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *blocker)
            .await?;
        let mut step =
            Box::pin(feature.terminal_step(&mut owner, &lane, &mut snapshot, OLD_ROLE, false));
        tokio::select! {
            result = step.as_mut() => panic!("step completed before journal wait: {result:?}"),
            result = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let blocked: bool = sqlx::query_scalar(
                        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                         WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)))",
                    ).bind(blocker_pid).fetch_one(pool).await?;
                    if blocked {
                        return Ok::<(), sqlx::Error>(());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }) => result??,
        }
        clock.store(NOW + 300, Ordering::SeqCst);
        blocker.rollback().await?;
        let result = step.await;
        sqlx::raw_sql(
            "DROP TRIGGER terminal_fixture_journal_wait ON self_role_audit;
             DROP FUNCTION terminal_fixture_journal_wait();",
        )
        .execute(pool)
        .await?;
        assert!(matches!(result, Err(RuntimeError::Stale)));
        // The no-send fact is immutable, but both leases expired while the
        // journal waited. This former owner cannot retire its ticket afterward.
        let effects = terminal_evidence(pool, &audit, true, false).await?;
        assert_eq!(effects.attempted_removed_role_ids, [OLD_ROLE]); // journal committed, send refused
        let receipts = role_receipts(pool, &audit.event_id).await?;
        assert_eq!(
            receipts,
            [(
                owner.claim.generation(),
                OLD_ROLE.into(),
                false,
                true,
                "no_send".into(),
                None
            )]
        );
        assert_eq!(effects.attempted_added_role_ids, [OLD_ROLE]);
        assert!(effects.compensated_removed_role_ids.is_empty());
        assert_eq!(effects.unresolved_removed_role_ids, [OLD_ROLE]);
        assert_eq!(
            effects.unresolved_added_role_ids.contains(&OLD_ROLE.into()),
            inherited_pending
        );
        assert!(
            !feature
                .store
                .finish_superseded_repair(&owner.claim, &lane.claim)
                .await?
        );
        let retained = exchange_owner_state(pool, &audit.event_id).await?;
        assert!(feature
            .store
            .retire_terminal_receipts(&mut owner.claim, &lane.claim)
            .await?
            .is_none());
        assert_eq!(exchange_owner_state(pool, &audit.event_id).await?, retained);
        assert_eq!(role_receipts(pool, &audit.event_id).await?, receipts);
        let (mut replacement, replacement_lane) = terminal_fixture_owners(&feature, &panel).await?;
        assert_eq!(replacement.claim.generation(), owner.claim.generation() + 1);
        assert_eq!(replacement_lane.claim.generation, lane.claim.generation + 1);
        assert!(replacement.claim.exchange_pending());
        assert!(!feature.store.release_panel_claim(&lane.claim).await?);
        let retired = feature
            .store
            .retire_terminal_receipts(&mut replacement.claim, &replacement_lane.claim)
            .await?
            .unwrap();
        assert_eq!(retired.exchange_pending, inherited_pending);
        assert!(retired.effects.unresolved_removed_role_ids.is_empty());
        assert_eq!(
            retired
                .effects
                .unresolved_added_role_ids
                .contains(&OLD_ROLE.into()),
            inherited_pending
        );
        assert_eq!(retired.effects.attempted_removed_role_ids, [OLD_ROLE]);
        assert_eq!(retired.effects.attempted_added_role_ids, [OLD_ROLE]);
        assert!(retired.effects.compensated_removed_role_ids.is_empty());
        assert_eq!(
            terminal_evidence(pool, &audit, inherited_pending, false).await?,
            retired.effects
        );
        assert_eq!(role_receipts(pool, &audit.event_id).await?, receipts);
        assert!(
            feature
                .store
                .owns_superseded_claim(&replacement.claim)
                .await?
        );
        assert!(
            feature
                .store
                .owns_panel_claim(&replacement_lane.claim)
                .await?
        );
        assert!(
            feature
                .store
                .release_panel_claim(&replacement_lane.claim)
                .await?
        );
        assert_terminal_winner(pool, &panel, &winner, Some("new")).await?;
        assert_eq!(mock.requests().len(), 4);
        assert!(mock.requests().iter().all(|r| r.method == "GET"));
        mock.shutdown().await;
    }
    Ok(())
}

async fn mixed_terminal_processing_sweep_and_dry_run(pool: &PgPool) -> TestResult {
    use crate::self_role_handlers::SelfRoleService;
    use two_bot_core::self_roles::SelfRoleGates;

    for dry_run in [false, true] {
        let clock = Arc::new(AtomicI64::new(NOW));
        let mock = MockRest::start(vec![], ScriptedResponse::status(500)).await;
        let feature = runtime(pool, &clock, &mock);
        let mut panel = panel(PanelMode::Select);
        panel.id = format!("mixed-terminal-sweep-{dry_run}");
        for n in 0..3 {
            let audit = terminal_seed(
                &feature.store,
                &panel,
                &format!("{}-terminal-{n}", panel.id),
                false,
            )
            .await?;
            sqlx::query("UPDATE self_role_audit SET intent_initialized=FALSE WHERE event_id=$1")
                .bind(&audit.event_id)
                .execute(pool)
                .await?;
            let mut processing = audit;
            processing.event_id = format!("{}-processing-{n}", panel.id);
            processing.effects = AuditEffects::default();
            processing.desired_role_ids.clear();
            assert!(feature
                .store
                .claim_pending_audit(&processing)
                .await?
                .is_some());
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
        assert_eq!(
            service.recover_once().await.unwrap(),
            if dry_run { 3 } else { 4 }
        );
        let (processing, repairs): (i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE outcome='processing'),
             count(*) FILTER (WHERE repair_expires_at IS NOT NULL)
             FROM self_role_audit WHERE panel_id=$1",
        )
        .bind(&panel.id)
        .fetch_one(pool)
        .await?;
        assert_eq!(processing, if dry_run { 0 } else { 1 });
        assert_eq!(repairs, if dry_run { 0 } else { 2 }); // two slots per queue, not four each
        assert_eq!(
            service.recover_once().await.unwrap(),
            if dry_run { 0 } else { 2 }
        );
        assert_eq!(service.recover_once().await.unwrap(), 0);
        let (generation, receipts): (i32, i64) = sqlx::query_as(
            "SELECT min(claim_generation),count(*) FILTER (WHERE repair_complete)
             FROM self_role_audit WHERE panel_id=$1 AND code='superseded_by_later_event'",
        )
        .bind(&panel.id)
        .fetch_one(pool)
        .await?;
        assert_eq!(generation, if dry_run { 1 } else { 2 });
        assert_eq!(receipts, 0);
        assert!(mock.requests().is_empty()); // invalid intent/dry-run never enter REST
        mock.shutdown().await;
    }
    Ok(())
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
    for (status, body_fault) in [
        (204, "none"),
        (403, "none"),
        (503, "none"),
        (403, "truncated"),
        (403, "stalled"),
        (503, "truncated"),
        (503, "stalled"),
    ] {
        let clock = Arc::new(AtomicI64::new(NOW));
        let mut script = snapshot(&[OLD_ROLE, OTHER]); // prepare
        script.extend(snapshot(&[OLD_ROLE, OTHER])); // before DELETE
        script.push(ScriptedResponse::status(204));
        script.extend(snapshot(&[OTHER])); // before PUT
        let response = if status == 204 {
            ScriptedResponse::status(status)
        } else {
            ScriptedResponse::json(status, json!({"message":"provider-secret"}))
        };
        script.push(match body_fault {
            "truncated" => response.truncated_body(),
            "stalled" => response.delayed_body(Duration::from_secs(6)),
            _ => response,
        });
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
        panel.id = format!("execution-{status}-{body_fault}");
        let request = request(
            &format!("execution-{status}-{body_fault}"),
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
        assert!(
            !prepared.event.exchange_pending,
            "received status is not a lost response"
        );
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
        // Lane-only compatibility repair has no typed ticket migration. Once
        // tracked attribution exists, its journal pins unknown legacy provenance;
        // observed committed-target compensation cannot retire that durable floor.
        assert!(pending);
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
