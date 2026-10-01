//! Hermetic V10a acceptance cases against the public core API.

use two_bot_core::health::{VoiceDiagnostic, VoicePermission, VoicePermissionScope};
use two_bot_core::voice_permission_health::{
    evaluate_permissions, notice_target, resolve_effective_permissions, NoticeCandidates,
    NoticeTarget, NoticeThrottle, OverwriteMasks, OverwriteTarget, PermissionFinding,
    PermissionOverwrite, TrackedFailure, NOTICE_BACKOFF_MS, NOTICE_MAX_SENDS,
    PERM_ADMINISTRATOR, PERM_MANAGE_CHANNELS, PERM_MANAGE_ROLES, PERM_MOVE_MEMBERS,
    PERM_VIEW_CHANNEL,
};

const CATEGORY: u64 = 10;
const CHANNEL: u64 = 20;
const BOT: u64 = 7;
const ROLE: u64 = 99;

fn masks(allow: u64, deny: u64) -> OverwriteMasks {
    OverwriteMasks { allow, deny }
}

fn overwrite(target: OverwriteTarget, allow: u64, deny: u64) -> PermissionOverwrite {
    PermissionOverwrite { target, allow, deny }
}

fn evaluate(
    guild_perms: u64,
    category_overrides: &[PermissionOverwrite],
    channel_overrides: &[PermissionOverwrite],
) -> Vec<PermissionFinding> {
    evaluate_permissions(
        guild_perms,
        CATEGORY,
        category_overrides,
        CHANNEL,
        channel_overrides,
        BOT,
        &[ROLE],
    )
}

fn all_required() -> u64 {
    PERM_MANAGE_CHANNELS | PERM_MOVE_MEMBERS | PERM_MANAGE_ROLES | PERM_VIEW_CHANNEL
}

// --- Resolution order table (criterion 2): base, @everyone, roles, member ---

#[test]
fn resolution_table_covers_discord_order_and_administrator() {
    struct Case {
        name: &'static str,
        base: u64,
        everyone: Option<OverwriteMasks>,
        roles: Vec<OverwriteMasks>,
        member: Option<OverwriteMasks>,
        expect: u64,
    }
    let view = PERM_VIEW_CHANNEL;
    let manage = PERM_MANAGE_CHANNELS;
    let cases = [
        Case {
            name: "empty stays empty",
            base: 0,
            everyone: None,
            roles: vec![],
            member: None,
            expect: 0,
        },
        Case {
            name: "base passes through",
            base: view,
            everyone: None,
            roles: vec![],
            member: None,
            expect: view,
        },
        Case {
            name: "everyone allow adds",
            base: 0,
            everyone: Some(masks(view, 0)),
            roles: vec![],
            member: None,
            expect: view,
        },
        Case {
            name: "everyone deny removes",
            base: view,
            everyone: Some(masks(0, view)),
            roles: vec![],
            member: None,
            expect: 0,
        },
        Case {
            name: "role allow adds",
            base: 0,
            everyone: None,
            roles: vec![masks(view, 0)],
            member: None,
            expect: view,
        },
        Case {
            name: "role deny beats base when no allow",
            base: view | manage,
            everyone: None,
            roles: vec![masks(0, view)],
            member: None,
            expect: manage,
        },
        Case {
            name: "role allow beats role deny",
            base: 0,
            everyone: None,
            roles: vec![masks(0, view), masks(view, 0)],
            member: None,
            expect: view,
        },
        Case {
            name: "role deny beats everyone allow",
            base: 0,
            everyone: Some(masks(view, 0)),
            roles: vec![masks(0, view)],
            member: None,
            expect: 0,
        },
        Case {
            name: "member allow beats role deny",
            base: 0,
            everyone: None,
            roles: vec![masks(0, view)],
            member: Some(masks(view, 0)),
            expect: view,
        },
        Case {
            name: "member deny beats role allow",
            base: view,
            everyone: None,
            roles: vec![masks(view, 0)],
            member: Some(masks(0, view)),
            expect: 0,
        },
        Case {
            name: "member wins without roles",
            base: view,
            everyone: None,
            roles: vec![],
            member: Some(masks(0, view)),
            expect: 0,
        },
        Case {
            name: "deny before allow within same row",
            base: 0,
            everyone: Some(masks(view, view)),
            roles: vec![],
            member: None,
            expect: view,
        },
        Case {
            name: "guild administrator bypasses all denies",
            base: PERM_ADMINISTRATOR,
            everyone: Some(masks(0, u64::MAX)),
            roles: vec![masks(0, u64::MAX)],
            member: Some(masks(0, u64::MAX)),
            expect: u64::MAX,
        },
        Case {
            name: "administrator from overwrite grants everything",
            base: 0,
            everyone: None,
            roles: vec![masks(PERM_ADMINISTRATOR, 0)],
            member: None,
            expect: u64::MAX,
        },
    ];
    assert!(cases.len() >= 12, "table must cover at least 12 cases");
    for case in cases {
        assert_eq!(
            resolve_effective_permissions(case.base, case.everyone, &case.roles, case.member),
            case.expect,
            "case: {}",
            case.name,
        );
    }
}

// --- evaluate_permissions (criterion 1) ---

#[test]
fn full_guild_base_with_no_overrides_is_healthy() {
    assert!(evaluate(all_required(), &[], &[]).is_empty());
}

#[test]
fn missing_guild_base_reports_all_four_at_guild_scope() {
    let findings = evaluate(0, &[], &[]);
    assert_eq!(findings.len(), 4);
    for finding in &findings {
        assert_eq!(finding.scope, VoicePermissionScope::Guild);
        assert_eq!(finding.category_id, None);
        assert_eq!(finding.channel_id, None);
    }
    let permissions: Vec<_> = findings.iter().map(|f| f.permission).collect();
    assert!(permissions.contains(&VoicePermission::ManageChannels));
    assert!(permissions.contains(&VoicePermission::MoveMembers));
    assert!(permissions.contains(&VoicePermission::ManageRoles));
    assert!(permissions.contains(&VoicePermission::ViewChannel));
}

#[test]
fn category_override_cause_names_the_category() {
    let findings = evaluate(
        all_required(),
        &[overwrite(OverwriteTarget::Everyone, 0, PERM_MOVE_MEMBERS)],
        &[],
    );
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].permission, VoicePermission::MoveMembers);
    assert_eq!(findings[0].scope, VoicePermissionScope::Category);
    assert_eq!(findings[0].category_id, Some(CATEGORY));
    assert_eq!(findings[0].channel_id, None);
}

#[test]
fn channel_override_cause_names_the_channel() {
    let findings = evaluate(
        all_required(),
        &[],
        &[overwrite(OverwriteTarget::Member(BOT), 0, PERM_MANAGE_ROLES)],
    );
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].permission, VoicePermission::ManageRoles);
    assert_eq!(findings[0].scope, VoicePermissionScope::Channel);
    assert_eq!(findings[0].category_id, None);
    assert_eq!(findings[0].channel_id, Some(CHANNEL));
}

#[test]
fn guild_shortage_is_attributed_to_guild_not_overwrites() {
    let findings = evaluate(
        all_required() & !PERM_VIEW_CHANNEL,
        &[overwrite(OverwriteTarget::Everyone, 0, PERM_VIEW_CHANNEL)],
        &[overwrite(OverwriteTarget::Everyone, 0, PERM_VIEW_CHANNEL)],
    );
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].scope, VoicePermissionScope::Guild);
    assert_eq!(findings[0].category_id, None);
    assert_eq!(findings[0].channel_id, None);
}

#[test]
fn overwrite_allow_rescues_missing_guild_base() {
    let findings = evaluate(
        all_required() & !PERM_MANAGE_CHANNELS,
        &[overwrite(OverwriteTarget::Role(ROLE), PERM_MANAGE_CHANNELS, 0)],
        &[],
    );
    assert!(findings.is_empty());
}

#[test]
fn channel_overwrite_only_applies_after_category_level() {
    // Category already removed Move Members: the finding stays category
    // scope even though the channel overwrite also denies it.
    let findings = evaluate(
        all_required(),
        &[overwrite(OverwriteTarget::Everyone, 0, PERM_MOVE_MEMBERS)],
        &[overwrite(OverwriteTarget::Everyone, 0, PERM_MOVE_MEMBERS)],
    );
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].scope, VoicePermissionScope::Category);
}

#[test]
fn foreign_role_and_member_overwrites_are_ignored() {
    let findings = evaluate(
        all_required(),
        &[overwrite(OverwriteTarget::Role(1000), 0, PERM_VIEW_CHANNEL)],
        &[overwrite(OverwriteTarget::Member(1001), 0, PERM_VIEW_CHANNEL)],
    );
    assert!(findings.is_empty());
}

#[test]
fn administrator_guild_base_yields_no_findings() {
    let findings = evaluate(
        PERM_ADMINISTRATOR,
        &[overwrite(OverwriteTarget::Everyone, 0, u64::MAX)],
        &[overwrite(OverwriteTarget::Everyone, 0, u64::MAX)],
    );
    assert!(findings.is_empty());
}

#[test]
fn findings_carry_only_enums_and_ids() {
    let findings = evaluate(
        0,
        &[overwrite(OverwriteTarget::Everyone, 0, PERM_MOVE_MEMBERS)],
        &[overwrite(OverwriteTarget::Member(BOT), 0, PERM_VIEW_CHANNEL)],
    );
    assert!(!findings.is_empty());
    for finding in &findings {
        let value = serde_json::to_value(finding).unwrap();
        let object = value.as_object().unwrap();
        for key in object.keys() {
            assert!(
                ["permission", "scope", "category_id", "channel_id"].contains(&key.as_str()),
                "unexpected key {key}"
            );
        }
        let rendered = format!("{finding:?} {value}");
        for forbidden in ["@everyone", "Authorization", "postgres://", "https://"] {
            assert!(!rendered.contains(forbidden), "leaked {forbidden}");
        }
    }
}

// --- notice_target fallback order (criterion 3) ---

fn candidates() -> NoticeCandidates {
    NoticeCandidates {
        system_channel_id: Some(100),
        setup_user_id: Some(7),
        setup_user_dm_reachable: true,
        owner_id: Some(9),
        owner_dm_reachable: true,
        creator_channel_id: Some(300),
    }
}

#[test]
fn notice_prefers_system_channel_with_setup_user_mention() {
    assert_eq!(
        notice_target(candidates()),
        Some(NoticeTarget::SystemChannel {
            channel_id: 100,
            mention_user_id: Some(7),
        })
    );
}

#[test]
fn notice_system_channel_without_setup_user_still_posts() {
    let mut input = candidates();
    input.setup_user_id = None;
    input.setup_user_dm_reachable = false;
    assert_eq!(
        notice_target(input),
        Some(NoticeTarget::SystemChannel {
            channel_id: 100,
            mention_user_id: None,
        })
    );
}

#[test]
fn notice_falls_back_to_setup_user_then_owner_then_creator() {
    let mut input = candidates();
    input.system_channel_id = None;
    assert_eq!(notice_target(input), Some(NoticeTarget::UserDm { user_id: 7 }));
    input.setup_user_dm_reachable = false;
    assert_eq!(notice_target(input), Some(NoticeTarget::UserDm { user_id: 9 }));
    input.owner_dm_reachable = false;
    assert_eq!(
        notice_target(input),
        Some(NoticeTarget::CreatorChannel { channel_id: 300 })
    );
}

#[test]
fn notice_returns_none_when_nothing_is_available() {
    assert_eq!(
        notice_target(NoticeCandidates {
            system_channel_id: None,
            setup_user_id: None,
            setup_user_dm_reachable: false,
            owner_id: None,
            owner_dm_reachable: false,
            creator_channel_id: None,
        }),
        None
    );
    // Unreachable DMs do not count as available.
    assert_eq!(
        notice_target(NoticeCandidates {
            system_channel_id: None,
            setup_user_id: Some(7),
            setup_user_dm_reachable: false,
            owner_id: Some(9),
            owner_dm_reachable: false,
            creator_channel_id: None,
        }),
        None
    );
}

// --- NoticeThrottle (criterion 4): N sends with backoff, then stop ---

fn tracked() -> TrackedFailure {
    TrackedFailure {
        guild_id: 1,
        location_id: Some(CATEGORY),
        diagnostic: VoiceDiagnostic::MissingPermission {
            permission: VoicePermission::ManageChannels,
            scope: VoicePermissionScope::Category,
        },
    }
}

#[test]
fn throttle_budget_and_backoff_are_documented() {
    assert_eq!(NOTICE_MAX_SENDS, 3);
    assert_eq!(
        NOTICE_BACKOFF_MS,
        [0, 5 * 60 * 1_000, 30 * 60 * 1_000],
        "immediate first notice, 5m then 30m repeats"
    );
}

#[test]
fn throttle_sends_a_few_times_with_backoff_then_stops() {
    let failure = tracked();
    let mut throttle = NoticeThrottle::new();
    assert!(throttle.should_notify(failure, 0));
    throttle.record_sent(failure, 0);
    // First repeat is not due before 5 minutes.
    assert!(!throttle.should_notify(failure, NOTICE_BACKOFF_MS[1] - 1));
    assert!(throttle.should_notify(failure, NOTICE_BACKOFF_MS[1]));
    throttle.record_sent(failure, NOTICE_BACKOFF_MS[1]);
    // Second repeat is not due before 30 more minutes.
    let second = NOTICE_BACKOFF_MS[1] + NOTICE_BACKOFF_MS[2];
    assert!(!throttle.should_notify(failure, second - 1));
    assert!(throttle.should_notify(failure, second));
    throttle.record_sent(failure, second);
    // Budget exhausted: silent afterwards, however late.
    assert!(!throttle.should_notify(failure, second + 7 * 24 * 60 * 60 * 1_000));
    // The failure stays listed for /setup until resolved.
    assert_eq!(throttle.current_failures(), vec![failure]);
}

#[test]
fn throttle_resolve_starts_a_fresh_budget() {
    let failure = tracked();
    let mut throttle = NoticeThrottle::new();
    for now in [0, NOTICE_BACKOFF_MS[1], NOTICE_BACKOFF_MS[1] + NOTICE_BACKOFF_MS[2]] {
        throttle.record_sent(failure, now);
    }
    assert!(!throttle.should_notify(failure, u64::MAX));
    assert!(throttle.resolve(failure));
    assert!(!throttle.resolve(failure));
    assert!(throttle.current_failures().is_empty());
    assert!(throttle.should_notify(failure, u64::MAX));
}

#[test]
fn throttle_lists_failures_in_deterministic_order() {
    let mut throttle = NoticeThrottle::new();
    let other = TrackedFailure {
        guild_id: 1,
        location_id: None,
        diagnostic: VoiceDiagnostic::RateLimited,
    };
    let failure = tracked();
    throttle.record_sent(other, 5);
    throttle.record_sent(failure, 0);
    // BTreeMap key order: guild, then location (None sorts before Some),
    // then diagnostic — insertion order does not matter.
    assert_eq!(throttle.current_failures(), vec![other, failure]);
    assert_eq!(throttle.clear_guild(1), 2);
    assert!(throttle.current_failures().is_empty());
}
