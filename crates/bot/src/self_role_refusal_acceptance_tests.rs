//! Protected behavioral contract for safe, actionable pre-mutation refusals.
//! Only the remote Discord boundary is doubled: dispatch, admission, leases,
//! selection storage, singular role execution and ephemeral edits are real.

#![cfg(test)]

use super::*;
use crate::{command_runtime::CommandRuntime, self_role_handlers::tests::component};
use two_bot_core::{InteractionRouter, RouterGates};

const EVENT: u64 = 100_000_000_000_001_100;
const HOSTILE: &str = "fixture-private-diagnostic @everyone <@100000000000000002>";

#[derive(Clone, Copy)]
enum Policy {
    Healthy,
    MissingManageRoles,
    MissingRole,
    AboveBot,
    EqualToBot,
    ManagedRole,
    UnsafePermission,
    Unavailable,
    LostPolicyAfterRemoval,
    UnknownSend,
}

struct RefusalFixture {
    pool: PgPool,
    clock: Arc<AtomicI64>,
    guild: Guild,
    mock: MockRest,
}

impl RefusalFixture {
    async fn new(pool: PgPool, policy: Policy, ack_status: u16) -> TestResult<Self> {
        // Existing runtime fixtures use the 0200-0206 store. Include the later
        // receipt-outcome columns too, so timeout controls use the real schema.
        sqlx::raw_sql(include_str!(
            "../../cutover/migrations/0449_self_role_ticket_acknowledgement.sql"
        ))
        .execute(&pool)
        .await?;
        // Service admission pins the settings revision before inserting durable
        // work, and fails closed when the revision row is missing. Seed the
        // settings store so full-dispatch tests admit against revision 0.
        sqlx::raw_sql(include_str!(
            "../../cutover/migrations/0330_guild_settings.sql"
        ))
        .execute(&pool)
        .await?;
        let guild = Guild::new(&[ROLE_B, UNRELATED]);
        let state = guild.0.clone();
        let mock = MockRest::with_responder(move |request| {
            if request.method == "POST" && request.path.ends_with("/callback") {
                return ScriptedResponse::status(ack_status);
            }
            if request.method == "PATCH" && request.path.ends_with("/messages/@original") {
                return ScriptedResponse::json(200, json!({}));
            }
            let active = if matches!(policy, Policy::LostPolicyAfterRemoval) {
                if state.lock().unwrap().held.contains(ROLE_B) {
                    Policy::Healthy
                } else {
                    Policy::MissingManageRoles
                }
            } else {
                policy
            };
            if matches!(active, Policy::Unavailable)
                && request.method == "GET"
                && request.path.ends_with(&format!("/members/{BOT}"))
            {
                return ScriptedResponse::json(503, json!({"message":HOSTILE.repeat(100)}));
            }
            let mut response = respond(&state, request);
            if request.method == "GET" && request.path.ends_with(&format!("/guilds/{GUILD}/roles"))
            {
                let mut roles: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
                let roles = roles.as_array_mut().unwrap();
                if matches!(active, Policy::MissingRole) {
                    roles.retain(|role| role["id"] != ROLE_A);
                }
                for role in roles.iter_mut() {
                    role["name"] = json!(HOSTILE);
                    if role["id"] == BOT_ROLE && matches!(active, Policy::MissingManageRoles) {
                        role["permissions"] = json!("0");
                    }
                    if role["id"] == ROLE_A {
                        match active {
                            Policy::AboveBot => role["position"] = json!(11),
                            Policy::EqualToBot => role["position"] = json!(10),
                            Policy::ManagedRole => role["managed"] = json!(true),
                            Policy::UnsafePermission => role["permissions"] = json!("8"),
                            _ => {}
                        }
                    }
                }
                response.body = serde_json::to_vec(roles).unwrap();
            }
            if matches!(active, Policy::UnknownSend)
                && request.method == "PUT"
                && request.path.ends_with(&format!("/roles/{ROLE_A}"))
            {
                // The server applies the request but no response arrives within
                // the executor's timeout. A subsequent snapshot is not a receipt.
                response = response.delayed(Duration::from_secs(2));
            }
            response
        })
        .await;
        Ok(Self {
            pool,
            clock: Arc::new(AtomicI64::new(NOW)),
            guild,
            mock,
        })
    }

    fn runtime(&self) -> SelfRoleRuntime {
        let mut feature = runtime(&self.pool, &self.clock, &self.mock);
        feature.executor = ActionExecutor::with_proxy_and_moderation_timeout(
            "fixture-token".into(),
            Some(self.mock.origin()),
            Duration::from_millis(500),
        )
        .unwrap();
        feature
    }

    async fn seed_selection(&self, panel: &SelfRolePanel) -> TestResult {
        let feature = self.runtime();
        let key = PanelKey {
            guild_id: GUILD.into(),
            member_id: USER.into(),
            panel_id: panel.id.clone(),
        };
        let seed_id = "100000000000000100";
        let order = event_order_for_event_id(seed_id, NOW as u64);
        let PanelClaimResult::Acquired(mut lane) = feature
            .store
            .claim_panel(&key, Some((seed_id, &order)))
            .await?
        else {
            return Err("fixture seed did not acquire the panel".into());
        };
        assert!(
            feature
                .store
                .set_panel_claim_option(&mut lane, Some("bravo"))
                .await?
        );
        assert!(feature.store.release_panel_claim(&lane).await?);
        Ok(())
    }

    fn command(&self, panel: &SelfRolePanel) -> Arc<CommandRuntime> {
        let feature = self.runtime();
        let executor = feature.executor.clone();
        let service = Arc::new(recovery_service(feature, panel).unwrap());
        let router = InteractionRouter::new(RouterGates {
            configured_guild: Some(GUILD.parse().unwrap()),
            scorecard: false,
            automations: false,
            announcements: false,
            moderation: false,
            voice: false,
            voice_assistant: false,
            tickets: false,
            self_roles: true,
            onboarding_picker: false,
            session_picker: false,
        });
        CommandRuntime::new_with_self_roles(
            self.pool.clone(),
            executor,
            router,
            GUILD.parse().unwrap(),
            false,
            service,
        )
    }

    fn role_writes(&self) -> Vec<(String, String)> {
        self.mock
            .requests()
            .into_iter()
            .filter(|call| {
                call.path.contains(&format!("/guilds/{GUILD}/members/")) && call.method != "GET"
            })
            .map(|call| (call.method, call.path))
            .collect()
    }

    fn first_reply(&self) -> String {
        let calls = self.mock.requests();
        let ack = calls
            .first()
            .expect("component must acknowledge before admission");
        assert_eq!(ack.method, "POST");
        assert!(ack.path.ends_with("/callback"));
        let ack: serde_json::Value = serde_json::from_slice(&ack.body).unwrap();
        assert_eq!(ack["type"], 5);
        assert_eq!(ack["data"]["flags"], 64, "defer must stay ephemeral");
        let edits: Vec<_> = calls.iter().filter(|call| call.method == "PATCH").collect();
        let reply = edits
            .first()
            .expect("the deferred original must receive a reply");
        assert!(reply.path.ends_with("/messages/@original"));
        let body: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(body["allowed_mentions"]["parse"], json!([]));
        assert!(body["allowed_mentions"]["users"]
            .as_array()
            .is_none_or(Vec::is_empty));
        assert!(body["allowed_mentions"]["roles"]
            .as_array()
            .is_none_or(Vec::is_empty));
        let content = body["content"].as_str().unwrap();
        assert!(!content.is_empty());
        assert!(content.encode_utf16().count() <= 2000);
        for private in [
            HOSTILE,
            "fixture-private-diagnostic",
            GUILD,
            USER,
            BOT,
            ROLE_A,
            ROLE_B,
            BOT_ROLE,
            CHANNEL,
            MESSAGE,
            "unsafe_live_roles",
            "missing_manage_roles",
            "role_hierarchy",
            "missing_role",
        ] {
            assert!(
                !content.contains(private),
                "reply exposed private diagnostic {private}"
            );
        }
        assert!(!content.contains("@everyone") && !content.contains("<@"));
        content.into()
    }

    async fn assert_clean_refusal(&self, panel: &SelfRolePanel, event: u64) -> TestResult {
        assert!(
            self.role_writes().is_empty(),
            "validation refusal must precede every role write"
        );
        assert_eq!(
            self.guild.held_labels(),
            ["B".into(), "UNRELATED".into()].into_iter().collect()
        );
        let row = stored(&self.pool, &event.to_string()).await?;
        assert_eq!(row.outcome, "rejected");
        assert!(!row.pending);
        assert_eq!(row.effects, AuditEffects::default());
        assert!(receipt_roles(&self.pool, &event.to_string())
            .await?
            .is_empty());
        assert_eq!(
            lane_target(&self.pool, &panel.id).await?,
            (Some("bravo".into()), true)
        );
        Ok(())
    }
}

fn picker() -> SelfRolePanel {
    catalogue(PanelMode::Select, true, "safe-refusal")
}

fn assert_contact_admin(reply: &str) {
    let lower = reply.to_lowercase();
    assert!(
        lower.contains("admin"),
        "member needs a contact-admin next action: {reply}"
    );
    assert!(
        ["contact", "ask"].iter().any(|word| lower.contains(word)),
        "a reason alone is not an actionable refusal: {reply}"
    );
}

fn assert_generic(reply: &str) {
    let lower = reply.to_lowercase();
    assert_contact_admin(reply);
    assert!(
        lower.contains("retry") || lower.contains("try again"),
        "unknown/unavailable needs a safe retry: {reply}"
    );
    for definitive in [
        "no longer exists",
        "hierarchy is wrong",
        "cannot manage roles in this server",
        "no roles were changed",
    ] {
        assert!(
            !lower.contains(definitive),
            "unknown work must not become a definitive validation refusal: {reply}"
        );
    }
}

async fn runtime_category(pool: PgPool, policy: Policy, expected: &str) -> TestResult {
    let fx = RefusalFixture::new(pool, policy, 204).await?;
    let panel = picker();
    fx.seed_selection(&panel).await?;
    let admission = fx
        .runtime()
        .prepare(&choose(&EVENT.to_string(), &["alpha"]), &panel)
        .await;
    fx.assert_clean_refusal(&panel, EVENT).await?;
    let code = match admission {
        Ok(Admission::Rejected(code)) => code.to_string(),
        _ => return Err("unsafe policy must produce a bounded pre-mutation refusal".into()),
    };
    assert_eq!(
        code, expected,
        "validation category must survive runtime admission"
    );
    assert_eq!(
        stored(&fx.pool, &EVENT.to_string()).await?.code.as_deref(),
        Some(expected)
    );
    fx.mock.shutdown().await;
    Ok(())
}

async fn component_category(
    pool: PgPool,
    policy: Policy,
    reason: &str,
    remediation: &[&str],
) -> TestResult {
    let fx = RefusalFixture::new(pool, policy, 204).await?;
    let panel = picker();
    fx.seed_selection(&panel).await?;
    fx.command(&panel)
        .on_interaction(&component(&panel, EVENT, vec!["alpha".into()]))
        .await;
    fx.assert_clean_refusal(&panel, EVENT).await?;
    let reply = fx.first_reply();
    assert!(
        reply.contains(reason),
        "vetted public validation reason was lost before the ephemeral edit: {reply}"
    );
    assert_contact_admin(&reply);
    let lower = reply.to_lowercase();
    for word in remediation {
        assert!(
            lower.contains(word),
            "admin remediation must explain {word}: {reply}"
        );
    }
    let actions: &[&str] = match policy {
        Policy::MissingManageRoles => &["grant", "give", "enable"],
        Policy::MissingRole => &["update", "replace", "reconfigure", "remove"],
        Policy::AboveBot | Policy::EqualToBot => &["above", "below", "higher", "move"],
        // Moving a managed role cannot make it editable. Contact staff without
        // promising that a hierarchy reorder can remove an integration fence.
        _ => &[],
    };
    assert!(
        actions.is_empty() || actions.iter().any(|action| lower.contains(action)),
        "admin remediation needs a feasible corrective action: {reply}"
    );
    fx.mock.shutdown().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn runtime_missing_manage_roles_keeps_specific_category() -> TestResult {
    isolated(|pool| async move {
        runtime_category(pool, Policy::MissingManageRoles, "missing_manage_roles").await
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn runtime_missing_configured_role_keeps_specific_category() -> TestResult {
    isolated(
        |pool| async move { runtime_category(pool, Policy::MissingRole, "missing_role").await },
    )
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn runtime_hierarchy_keeps_specific_category() -> TestResult {
    isolated(|pool| async move { runtime_category(pool, Policy::AboveBot, "role_hierarchy").await })
        .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn component_missing_manage_roles_explains_permission_and_next_action() -> TestResult {
    isolated(|pool| async move {
        component_category(
            pool,
            Policy::MissingManageRoles,
            "I cannot manage roles in this server.",
            &["manage roles"],
        )
        .await
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn component_missing_role_explains_configuration_and_next_action() -> TestResult {
    isolated(|pool| async move {
        component_category(
            pool,
            Policy::MissingRole,
            "That role no longer exists.",
            &["panel"],
        )
        .await
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn component_above_bot_explains_hierarchy_and_next_action() -> TestResult {
    isolated(|pool| async move {
        component_category(
            pool,
            Policy::AboveBot,
            "I cannot manage that role because the role hierarchy is wrong.",
            &["bot", "role"],
        )
        .await
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn component_equal_rank_preserves_hierarchy_refusal() -> TestResult {
    isolated(|pool| async move {
        component_category(
            pool,
            Policy::EqualToBot,
            "I cannot manage that role because the role hierarchy is wrong.",
            &["bot", "role"],
        )
        .await
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn component_managed_role_preserves_hierarchy_refusal() -> TestResult {
    isolated(|pool| async move {
        component_category(
            pool,
            Policy::ManagedRole,
            "I cannot manage that role because the role hierarchy is wrong.",
            &["role"],
        )
        .await
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn unknown_policy_uses_safe_generic_retry_and_admin_action() -> TestResult {
    isolated(|pool| async move {
        let fx = RefusalFixture::new(pool, Policy::UnsafePermission, 204).await?;
        let panel = picker();
        fx.seed_selection(&panel).await?;
        fx.command(&panel)
            .on_interaction(&component(&panel, EVENT, vec!["alpha".into()]))
            .await;
        fx.assert_clean_refusal(&panel, EVENT).await?;
        assert_generic(&fx.first_reply());
        fx.mock.shutdown().await;
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn unavailable_snapshot_uses_safe_generic_retry_without_raw_error() -> TestResult {
    isolated(|pool| async move {
        let fx = RefusalFixture::new(pool, Policy::Unavailable, 204).await?;
        let panel = picker();
        fx.seed_selection(&panel).await?;
        fx.command(&panel)
            .on_interaction(&component(&panel, EVENT, vec!["alpha".into()]))
            .await;
        assert!(fx.role_writes().is_empty());
        let row = stored(&fx.pool, &EVENT.to_string()).await?;
        assert_eq!(
            row.outcome, "processing",
            "failed read cannot prove a definitive refusal"
        );
        assert_eq!(row.effects, AuditEffects::default());
        assert_eq!(
            lane_target(&fx.pool, &panel.id).await?,
            (Some("bravo".into()), true)
        );
        assert_generic(&fx.first_reply());
        fx.mock.shutdown().await;
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn unavailable_store_uses_safe_generic_retry_without_raw_error() -> TestResult {
    isolated(|pool| async move {
        let fx = RefusalFixture::new(pool, Policy::Healthy, 204).await?;
        let panel = picker();
        fx.seed_selection(&panel).await?;
        // A real SQL error at admission, confined to this generated schema.
        // Its raw constraint name is deliberately not suitable for members.
        sqlx::raw_sql("ALTER TABLE self_role_audit ADD CONSTRAINT \"fixture-private-diagnostic\" CHECK (event_id <> '100000000000001100')")
            .execute(&fx.pool).await?;
        fx.command(&panel).on_interaction(&component(&panel, EVENT, vec!["alpha".into()])).await;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM self_role_audit").fetch_one(&fx.pool).await?;
        assert_eq!(count, 0, "failed admission must not fabricate a terminal audit");
        assert!(fx.role_writes().is_empty());
        assert_eq!(lane_target(&fx.pool, &panel.id).await?, (Some("bravo".into()), true));
        assert_generic(&fx.first_reply());
        fx.mock.shutdown().await;
        Ok(())
    }).await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn ambiguous_acknowledgement_never_admits_or_mutates_unchanged() -> TestResult {
    isolated(|pool| async move {
        // A received 503 does not prove whether Discord accepted the defer.
        // Without a confirmed acknowledgement the bot must do no role work.
        let fx = RefusalFixture::new(pool, Policy::MissingRole, 503).await?;
        let panel = picker();
        fx.command(&panel)
            .on_interaction(&component(&panel, EVENT, vec!["alpha".into()]))
            .await;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM self_role_audit")
            .fetch_one(&fx.pool)
            .await?;
        assert_eq!(count, 0);
        let calls = fx.mock.requests();
        assert_eq!(
            calls.len(),
            1,
            "ambiguous acknowledgement forbids validation, role work and a success reply"
        );
        assert_eq!(calls[0].method, "POST");
        assert!(calls[0].path.ends_with("/callback"));
        assert!(fx.role_writes().is_empty());
        fx.mock.shutdown().await;
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn successful_component_and_duplicate_fences_unchanged() -> TestResult {
    isolated(|pool| async move {
        let fx = RefusalFixture::new(pool, Policy::Healthy, 204).await?;
        let panel = picker();
        fx.seed_selection(&panel).await?;
        let command = fx.command(&panel);
        let input = component(&panel, EVENT, vec!["alpha".into()]);
        command.on_interaction(&input).await;
        assert_eq!(fx.first_reply(), "Role updated.");
        assert_eq!(
            fx.role_writes(),
            vec![
                (
                    "DELETE".into(),
                    format!("/api/v10/guilds/{GUILD}/members/{USER}/roles/{ROLE_B}")
                ),
                (
                    "PUT".into(),
                    format!("/api/v10/guilds/{GUILD}/members/{USER}/roles/{ROLE_A}")
                ),
            ]
        );
        let row = stored(&fx.pool, &EVENT.to_string()).await?;
        assert_eq!(row.outcome, "switched");
        assert!(!row.pending);
        assert_eq!(row.before, ids(&[ROLE_B]));
        assert_eq!(row.desired, ids(&[ROLE_A]));
        assert_eq!(
            lane_target(&fx.pool, &panel.id).await?,
            (Some("alpha".into()), true)
        );
        let before = fx.mock.requests().len();
        command.on_interaction(&input).await;
        let calls = fx.mock.requests();
        assert_eq!(
            calls.len(),
            before + 2,
            "duplicate only acknowledges and edits, without REST reads or writes"
        );
        let body: serde_json::Value = serde_json::from_slice(&calls.last().unwrap().body)?;
        assert_eq!(
            body["content"],
            "This selection is already being handled or was handled earlier."
        );
        fx.mock.shutdown().await;
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn failed_acknowledgement_never_admits_or_validates_unchanged() -> TestResult {
    isolated(|pool| async move {
        let fx = RefusalFixture::new(pool, Policy::MissingManageRoles, 403).await?;
        let panel = picker();
        fx.command(&panel)
            .on_interaction(&component(&panel, EVENT, vec!["alpha".into()]))
            .await;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM self_role_audit")
            .fetch_one(&fx.pool)
            .await?;
        assert_eq!(count, 0);
        let calls = fx.mock.requests();
        assert_eq!(
            calls.len(),
            1,
            "failed defer forbids admission, policy reads and role writes"
        );
        assert_eq!(calls[0].method, "POST");
        assert!(calls[0].path.ends_with("/callback"));
        assert!(fx.role_writes().is_empty());
        fx.mock.shutdown().await;
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn recovered_pending_evidence_is_not_a_definitive_refusal_unchanged() -> TestResult {
    isolated(|pool| async move {
        let fx = RefusalFixture::new(pool, Policy::AboveBot, 204).await?;
        let panel = picker();
        fx.seed_selection(&panel).await?;
        // A previous process left initialized intent and an unknown remote
        // exchange. Build it via real store APIs, not direct row fabrication.
        let feature = fx.runtime();
        let audit = SelfRoleAudit {
            event_id: EVENT.to_string(),
            event_order: Some(event_order_for_event_id(&EVENT.to_string(), NOW as u64)),
            guild_id: GUILD.into(),
            member_id: USER.into(),
            panel_id: panel.id.clone(),
            source: PanelMode::Select,
            source_id: MESSAGE.into(),
            option_key: None,
            role_id: None,
            operation: two_bot_core::self_roles::RoleOperation::Replace,
            outcome: SettledOutcome::Rejected,
            code: None,
            reason: None,
            effects: AuditEffects::default(),
            desired_role_ids: vec![],
            pre_mutation_role_ids: vec![],
        };
        let mut event = feature.store.claim_pending_audit(&audit).await?.unwrap();
        assert!(
            feature
                .store
                .initialize_intent(&mut event, &ids(&[ROLE_A]), &ids(&[ROLE_B]))
                .await?
        );
        let effects = AuditEffects {
            attempted_added_role_ids: ids(&[ROLE_A]),
            unresolved_added_role_ids: ids(&[ROLE_A]),
            ..AuditEffects::default()
        };
        assert!(
            feature
                .store
                .checkpoint_exchange(&event, &effects, false, Some(true))
                .await?
        );
        fx.clock.store(NOW + 500, Ordering::SeqCst);
        fx.command(&panel)
            .on_interaction(&component(&panel, EVENT, vec!["alpha".into()]))
            .await;
        let row = stored(&fx.pool, &EVENT.to_string()).await?;
        assert_eq!(row.outcome, "processing");
        assert!(row.pending);
        assert_eq!(row.effects, effects);
        assert_eq!(row.before, ids(&[ROLE_B]));
        assert_eq!(row.desired, ids(&[ROLE_A]));
        assert!(fx.role_writes().is_empty());
        assert_eq!(
            lane_target(&fx.pool, &panel.id).await?,
            (Some("bravo".into()), true)
        );
        let reply = fx.first_reply();
        // The re-press meets the still-claimed event, so dispatch keeps the
        // existing in-flight acknowledgement instead of a fresh verdict. The
        // reply must never relabel unknown evidence as a definitive refusal.
        assert!(
            reply.contains("already being handled"),
            "re-press of pending evidence keeps the in-flight acknowledgement, got: {reply}"
        );
        assert!(
            !reply.contains("could not be applied"),
            "pending evidence must not read as a definitive refusal, got: {reply}"
        );
        assert!(!reply.contains("hierarchy is wrong"));
        fx.mock.shutdown().await;
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn policy_failure_after_mutation_stays_unresolved_unchanged() -> TestResult {
    isolated(|pool| async move {
        let fx = RefusalFixture::new(pool, Policy::LostPolicyAfterRemoval, 204).await?;
        let panel = picker();
        fx.seed_selection(&panel).await?;
        fx.command(&panel)
            .on_interaction(&component(&panel, EVENT, vec!["alpha".into()]))
            .await;
        assert_eq!(
            fx.role_writes(),
            vec![(
                "DELETE".into(),
                format!("/api/v10/guilds/{GUILD}/members/{USER}/roles/{ROLE_B}")
            )]
        );
        let row = stored(&fx.pool, &EVENT.to_string()).await?;
        assert_eq!(row.outcome, "processing");
        assert_eq!(row.effects.attempted_removed_role_ids, ids(&[ROLE_B]));
        assert_eq!(row.before, ids(&[ROLE_B]));
        assert_eq!(row.desired, ids(&[ROLE_A]));
        assert_eq!(
            lane_target(&fx.pool, &panel.id).await?,
            (Some("bravo".into()), true)
        );
        let reply = fx.first_reply();
        assert!(reply.contains("unresolved"));
        assert!(!reply.contains("I cannot manage roles in this server."));
        fx.mock.shutdown().await;
        Ok(())
    })
    .await
}

#[tokio::test]
#[ignore = "requires isolated agent-testdb; run with --ignored"]
async fn timed_out_role_write_keeps_pending_receipt_unchanged() -> TestResult {
    isolated(|pool| async move {
        let fx = RefusalFixture::new(pool, Policy::UnknownSend, 204).await?;
        let panel = picker();
        fx.seed_selection(&panel).await?;
        fx.command(&panel).on_interaction(&component(&panel, EVENT, vec!["alpha".into()])).await;
        let row = stored(&fx.pool, &EVENT.to_string()).await?;
        assert_eq!(row.outcome, "processing");
        assert!(row.pending, "a snapshot or compensation cannot resolve an unknown send");
        assert!(row.effects.attempted_added_role_ids.contains(&ROLE_A.to_owned()));
        assert!(row.effects.unresolved_added_role_ids.contains(&ROLE_A.to_owned()));
        let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM self_role_exchanges WHERE event_id=$1 AND disposition='pending' AND completed_at IS NULL")
            .bind(EVENT.to_string()).fetch_one(&fx.pool).await?;
        assert_eq!(pending, 1, "timeout must not fabricate a received response");
        assert_eq!(lane_target(&fx.pool, &panel.id).await?, (Some("bravo".into()), true));
        let reply = fx.first_reply();
        assert!(reply.contains("unresolved"));
        assert!(!reply.contains("hierarchy is wrong") && !reply.contains("no longer exists"));
        fx.mock.shutdown().await;
        Ok(())
    }).await
}
