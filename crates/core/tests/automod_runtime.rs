use std::collections::{HashMap, HashSet};
use two_bot_core::automod_runtime::*;
use two_bot_core::commands::PERM_MODERATE_MEMBERS;
use two_bot_core::{
    AutomodConfig, AutomodMessage, ModerationPolicy, ModerationTarget, TargetProtection,
};

fn runtime(enforce: bool) -> AutomodRuntime {
    let mut vars = HashMap::from([
        ("TWO_AUTOMOD".into(), "1".into()),
        ("TWO_AUTOMOD_BAD_WORDS".into(), "blocked".into()),
    ]);
    if enforce {
        vars.insert("TWO_AUTOMOD_ENFORCE".into(), "1".into());
    }
    AutomodRuntime::new(
        AutomodConfig::from_map(&vars).unwrap(),
        AutomodScope {
            guild_id: STAGING_GUILD_ID.into(),
            live_approved: false,
        },
    )
}

fn delivery(kind: MessageDeliveryKind, content: &str) -> MessageDelivery {
    MessageDelivery {
        kind,
        guild_id: Some(STAGING_GUILD_ID.into()),
        channel_id: "222222222222222222".into(),
        message_id: "333333333333333333".into(),
        observed_timestamp_ms: 1_000_000,
        edited_timestamp_ms: None,
        snapshot: Some(AutomodMessage {
            guild_id: STAGING_GUILD_ID.into(),
            channel_id: "222222222222222222".into(),
            message_id: "333333333333333333".into(),
            author_id: "444444444444444444".into(),
            author_is_bot: false,
            role_ids: vec![],
            content: content.into(),
            mentioned_user_ids: vec![],
            attachment_names: vec![],
            observed_timestamp_ms: 1_000_000,
        }),
    }
}

fn facts() -> TargetFacts {
    TargetFacts {
        target: ModerationTarget {
            user_id: "444444444444444444".into(),
            role_ids: vec![],
            highest_role_position: 1,
            is_bot: false,
            is_guild_owner: false,
        },
        policy: ModerationPolicy {
            owen_user_id: "555555555555555555".into(),
            protected_role_ids: HashSet::from(["666666666666666666".into()]),
            bot_user_id: None,
        },
        bot_highest_role_position: 10,
        bot_permissions: PERM_MODERATE_MEMBERS,
    }
}

fn matched(runtime: &mut AutomodRuntime, delivery: &MessageDelivery) -> AutomodMatch {
    let Inspection::Matched(matched) = runtime.inspect(delivery) else {
        panic!("expected match")
    };
    matched
}

#[test]
fn create_routes_match_to_capture_only_and_clean_to_funnel() {
    let mut runtime = runtime(true);
    let matched = matched(
        &mut runtime,
        &delivery(MessageDeliveryKind::Create, "blocked"),
    );
    assert_eq!(matched.funnel, FunnelDisposition::CaptureOnly);
    assert_eq!(
        runtime.inspect(&delivery(MessageDeliveryKind::Create, "hello")),
        Inspection::Accepted(FunnelDisposition::Accept)
    );
}

#[test]
fn dry_run_has_no_effect_or_ledger_increment_and_needs_no_resolver() {
    let mut runtime = runtime(false);
    let matched = matched(
        &mut runtime,
        &delivery(MessageDeliveryKind::Create, "blocked"),
    );
    assert_eq!(runtime.target_gate(&matched, None), TargetGate::DryRun);
    for count in [0, 1, 2, 3, 99] {
        let plan = runtime.plan(
            &matched,
            ViolationRecord {
                count,
                inserted: true,
            },
            None,
        );
        assert_eq!(plan.outcome, PlanOutcome::DryRun);
        assert!(plan.effects.is_empty());
        assert_eq!(plan.violation_count, None);
        assert_eq!(plan.funnel, FunnelDisposition::CaptureOnly);
    }
}

#[test]
fn enforce_plans_exact_delete_then_ladder_effect() {
    let mut runtime = runtime(true);
    let matched = matched(
        &mut runtime,
        &delivery(MessageDeliveryKind::Create, "blocked"),
    );
    let facts = facts();
    assert_eq!(
        runtime
            .plan(
                &matched,
                ViolationRecord {
                    count: 1,
                    inserted: true
                },
                Some(&facts)
            )
            .effects,
        vec![AutomodEffect::DeleteMessage]
    );
    assert_eq!(
        runtime
            .plan(
                &matched,
                ViolationRecord {
                    count: 2,
                    inserted: true
                },
                Some(&facts)
            )
            .effects,
        vec![AutomodEffect::DeleteMessage, AutomodEffect::WarnMember]
    );
    assert_eq!(
        runtime
            .plan(
                &matched,
                ViolationRecord {
                    count: 3,
                    inserted: true
                },
                Some(&facts)
            )
            .effects,
        vec![
            AutomodEffect::DeleteMessage,
            AutomodEffect::TimeoutMember { seconds: 600 }
        ]
    );
}

#[test]
fn every_protected_target_is_untouched_even_on_delete_rung() {
    let mut runtime = runtime(true);
    let matched = matched(
        &mut runtime,
        &delivery(MessageDeliveryKind::Create, "blocked"),
    );
    for protection in [
        TargetProtection::GuildOwner,
        TargetProtection::Owen,
        TargetProtection::Bot,
        TargetProtection::StaffRole,
    ] {
        let mut facts = facts();
        match protection {
            TargetProtection::GuildOwner => facts.target.is_guild_owner = true,
            TargetProtection::Owen => facts.policy.owen_user_id = facts.target.user_id.clone(),
            TargetProtection::Bot => facts.target.is_bot = true,
            TargetProtection::StaffRole => facts.target.role_ids.push("666666666666666666".into()),
        }
        for count in [1, 2, 3] {
            let plan = runtime.plan(
                &matched,
                ViolationRecord {
                    count,
                    inserted: true,
                },
                Some(&facts),
            );
            assert_eq!(plan.outcome, PlanOutcome::Protected(protection));
            assert!(plan.effects.is_empty());
            assert_eq!(plan.violation_count, Some(count));
        }
    }
}

#[test]
fn unresolved_or_wrong_author_fails_closed_before_delete() {
    let mut runtime = runtime(true);
    let matched = matched(
        &mut runtime,
        &delivery(MessageDeliveryKind::Create, "blocked"),
    );
    let mut facts = facts();
    facts.target.user_id = "someone-else".into();
    for target in [None, Some(&facts)] {
        let plan = runtime.plan(
            &matched,
            ViolationRecord {
                count: 0,
                inserted: true,
            },
            target,
        );
        assert_eq!(plan.outcome, PlanOutcome::Unavailable);
        assert!(plan.effects.is_empty());
        assert_eq!(plan.violation_count, None);
        assert_eq!(plan.funnel, FunnelDisposition::CaptureOnly);
    }
}

#[test]
fn hierarchy_and_permission_gate_followup_sanction_not_exact_delete() {
    let mut runtime = runtime(true);
    let matched = matched(
        &mut runtime,
        &delivery(MessageDeliveryKind::Create, "blocked"),
    );
    let mut facts = facts();
    facts.target.highest_role_position = facts.bot_highest_role_position;
    let plan = runtime.plan(
        &matched,
        ViolationRecord {
            count: 3,
            inserted: true,
        },
        Some(&facts),
    );
    assert_eq!(plan.outcome, PlanOutcome::SanctionRefused);
    assert_eq!(plan.effects, vec![AutomodEffect::DeleteMessage]);
    facts.target.highest_role_position = 1;
    facts.bot_permissions = 0;
    assert_eq!(
        runtime
            .plan(
                &matched,
                ViolationRecord {
                    count: 2,
                    inserted: true
                },
                Some(&facts)
            )
            .outcome,
        PlanOutcome::SanctionRefused
    );
}

#[test]
fn partial_update_fetches_and_reinspects_without_funnel_or_xp() {
    let mut runtime = runtime(true);
    let mut edit = delivery(MessageDeliveryKind::Update, "blocked");
    let snapshot = edit.snapshot.take().unwrap();
    assert_eq!(
        runtime.inspect(&edit),
        Inspection::FetchMessage {
            guild_id: STAGING_GUILD_ID.into(),
            channel_id: edit.channel_id.clone(),
            message_id: edit.message_id.clone(),
        }
    );
    edit.snapshot = Some(snapshot);
    assert_eq!(matched(&mut runtime, &edit).funnel, FunnelDisposition::None);
    edit.snapshot.as_mut().unwrap().content = "hello".into();
    assert_eq!(
        runtime.inspect(&edit),
        Inspection::Accepted(FunnelDisposition::None)
    );
    edit.snapshot.as_mut().unwrap().message_id = "different".into();
    assert_eq!(runtime.inspect(&edit), Inspection::Unavailable);
}

#[test]
fn edits_use_receipt_time_and_do_not_count_same_message_twice_for_repeats() {
    let mut runtime = runtime(true);
    for id in ["1", "2"] {
        let mut msg = delivery(MessageDeliveryKind::Create, "same");
        msg.message_id = id.into();
        msg.snapshot.as_mut().unwrap().message_id = id.into();
        assert_eq!(
            runtime.inspect(&msg),
            Inspection::Accepted(FunnelDisposition::Accept)
        );
    }
    let mut edit = delivery(MessageDeliveryKind::Update, "same");
    edit.message_id = "2".into();
    edit.snapshot.as_mut().unwrap().message_id = "2".into();
    assert_eq!(
        runtime.inspect(&edit),
        Inspection::Accepted(FunnelDisposition::None)
    );
    edit.message_id = "3".into();
    edit.snapshot.as_mut().unwrap().message_id = "3".into();
    edit.observed_timestamp_ms += 31_000;
    assert_eq!(
        runtime.inspect(&edit),
        Inspection::Accepted(FunnelDisposition::None)
    );
}

#[test]
fn staging_fence_enforce_is_not_live_approval_and_dms_are_ignored() {
    let scope = AutomodScope {
        guild_id: LIVE_GUILD_ID.into(),
        live_approved: false,
    };
    assert!(!scope.permits(LIVE_GUILD_ID));
    let approved = AutomodScope {
        live_approved: true,
        ..scope
    };
    assert!(approved.permits(LIVE_GUILD_ID));
    assert!(!approved.permits(STAGING_GUILD_ID));
    assert!(!approved.permits("unknown"));
    let mut runtime = runtime(true);
    let mut msg = delivery(MessageDeliveryKind::Create, "blocked");
    msg.guild_id = None;
    assert_eq!(runtime.inspect(&msg), Inspection::Ignore);
    msg.guild_id = Some(LIVE_GUILD_ID.into());
    assert_eq!(runtime.inspect(&msg), Inspection::Ignore);
}

#[test]
fn keys_dedupe_receipt_retries_but_not_edit_revisions_or_modes() {
    let mut msg = delivery(MessageDeliveryKind::Create, "hello");
    let original = DeliveryKey::from_delivery(&msg, false).unwrap();
    msg.observed_timestamp_ms += 1_000;
    msg.snapshot.as_mut().unwrap().content = "blocked".into();
    assert_eq!(DeliveryKey::from_delivery(&msg, false).unwrap(), original);
    assert_ne!(DeliveryKey::from_delivery(&msg, true).unwrap(), original);
    msg.kind = MessageDeliveryKind::Update;
    msg.edited_timestamp_ms = Some(2_000_000);
    let edit = DeliveryKey::from_delivery(&msg, false).unwrap();
    assert_ne!(edit, original);
    msg.observed_timestamp_ms += 1_000;
    assert_eq!(DeliveryKey::from_delivery(&msg, false).unwrap(), edit);
    msg.edited_timestamp_ms = Some(2_001_000);
    assert_ne!(DeliveryKey::from_delivery(&msg, false).unwrap(), edit);
    msg.snapshot = None;
    assert!(DeliveryKey::from_delivery(&msg, false).is_none());
}

#[test]
fn already_counted_message_never_repeats_effects_on_a_different_edit() {
    let mut runtime = runtime(true);
    let matched = matched(
        &mut runtime,
        &delivery(MessageDeliveryKind::Update, "blocked"),
    );
    let plan = runtime.plan(
        &matched,
        ViolationRecord {
            count: 3,
            inserted: false,
        },
        Some(&facts()),
    );
    assert_eq!(plan.outcome, PlanOutcome::AlreadyProcessed);
    assert!(plan.effects.is_empty());
    assert_eq!(plan.funnel, FunnelDisposition::None);
}

#[test]
fn attachment_blocklist_and_exemptions_apply_in_runtime() {
    let mut runtime = runtime(true);
    let mut msg = delivery(MessageDeliveryKind::Create, "file");
    msg.snapshot.as_mut().unwrap().attachment_names = vec!["Setup.EXE".into()];
    assert_eq!(
        matched(&mut runtime, &msg).filter,
        two_bot_core::AutomodFilter::AttachmentType
    );
    msg.snapshot.as_mut().unwrap().author_is_bot = true;
    assert_eq!(
        runtime.inspect(&msg),
        Inspection::Accepted(FunnelDisposition::Accept)
    );
}
