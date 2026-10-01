//! Hermetic V8a acceptance cases against the public core API.
//!
//! Table tests pin the documented builder rules; property tests prove the
//! security contract: (a) no output grants a bit the source did not allow,
//! except the documented owner/required-role grants on their own targets,
//! (b) deny wins over allow on every output target, (c) no duplicate
//! targets. An extra property proves owner grants can never carry Manage
//! Roles or Administrator, however the caller sets `owner_extra_allow`.
//!
//! No tests in this fixture use a database, Redis, Discord, or a staging
//! identity.

use proptest::prelude::*;
use two_bot_core::voice_permissions::{
    plan_room_overrides, ChannelOverride, InheritanceSource, OverrideKind, PermissionPlanError,
    RoomPermissionInput, RoomPermissionPlan, OWNER_ALLOW_BITS, OWNER_EXTRA_MASK,
    PERM_ADMINISTRATOR, PERM_CONNECT, PERM_MANAGE_CHANNELS, PERM_MANAGE_ROLES, PERM_MOVE_MEMBERS,
    PERM_SPEAK, PERM_VIEW_CHANNEL, PRIVATE_EVERYONE_DENY, REQUIRED_ROLE_ALLOW_BITS,
};

const OWNER: u64 = 11;
const EVERYONE: u64 = 100;
const REQUIRED: u64 = 200;

fn base(private: bool) -> RoomPermissionInput<'static> {
    RoomPermissionInput {
        source: InheritanceSource::CreatorChannel,
        source_overrides: &[],
        bot_can_manage_roles: true,
        owner_id: OWNER,
        owner_extra_allow: 0,
        private,
        everyone_role_id: EVERYONE,
        required_role: None,
    }
}

fn role(id: u64, allow: u64, deny: u64) -> ChannelOverride {
    ChannelOverride {
        id,
        kind: OverrideKind::Role,
        allow,
        deny,
    }
}

fn member(id: u64, allow: u64, deny: u64) -> ChannelOverride {
    ChannelOverride {
        id,
        kind: OverrideKind::Member,
        allow,
        deny,
    }
}

fn overrides(plan: RoomPermissionPlan) -> Vec<ChannelOverride> {
    match plan {
        RoomPermissionPlan::Overrides(list) => list,
        RoomPermissionPlan::SyncToCategory => panic!("expected an override list"),
    }
}

fn find<'a>(list: &'a [ChannelOverride], id: u64, kind: OverrideKind) -> &'a ChannelOverride {
    list.iter()
        .find(|o| o.id == id && o.kind == kind)
        .unwrap_or_else(|| panic!("missing override for ({id}, {kind:?})"))
}

#[test]
fn without_manage_roles_syncs_to_category_without_validating_anything_else() {
    // Even zero IDs are accepted: the bot cannot legally set overrides, so
    // nothing else is inspected.
    let input = RoomPermissionInput {
        bot_can_manage_roles: false,
        owner_id: 0,
        everyone_role_id: 0,
        ..base(true)
    };
    assert_eq!(
        plan_room_overrides(&input),
        Ok(RoomPermissionPlan::SyncToCategory)
    );
}

#[test]
fn public_room_copies_source_and_adds_owner_grant() {
    let source = [
        role(EVERYONE, PERM_VIEW_CHANNEL | PERM_CONNECT, 0),
        role(300, PERM_VIEW_CHANNEL, PERM_CONNECT),
    ];
    let input = RoomPermissionInput {
        source_overrides: &source,
        ..base(false)
    };
    let list = overrides(plan_room_overrides(&input).unwrap());
    // Source overrides pass through verbatim.
    assert_eq!(
        find(&list, EVERYONE, OverrideKind::Role),
        &role(EVERYONE, PERM_VIEW_CHANNEL | PERM_CONNECT, 0)
    );
    assert_eq!(
        find(&list, 300, OverrideKind::Role),
        &role(300, PERM_VIEW_CHANNEL, PERM_CONNECT)
    );
    // The owner gets exactly the documented base bits on their own room.
    assert_eq!(
        find(&list, OWNER, OverrideKind::Member),
        &member(OWNER, OWNER_ALLOW_BITS, 0)
    );
    assert_eq!(list.len(), 3);
}

#[test]
fn private_room_denies_connect_to_everyone_keeps_view_owner_keeps_connect() {
    let source = [role(EVERYONE, PERM_VIEW_CHANNEL | PERM_CONNECT, 0)];
    let input = RoomPermissionInput {
        source_overrides: &source,
        ..base(true)
    };
    let list = overrides(plan_room_overrides(&input).unwrap());
    let everyone = find(&list, EVERYONE, OverrideKind::Role);
    assert_eq!(everyone.allow, PERM_VIEW_CHANNEL);
    assert_eq!(everyone.deny, PERM_CONNECT);
    let owner = find(&list, OWNER, OverrideKind::Member);
    assert_ne!(owner.allow & PERM_CONNECT, 0, "the owner keeps Connect");
    assert_ne!(owner.allow & PERM_VIEW_CHANNEL, 0, "the owner keeps View");
}

#[test]
fn private_room_without_source_everyone_entry_creates_deny_only_entry() {
    let input = base(true);
    let list = overrides(plan_room_overrides(&input).unwrap());
    assert_eq!(
        find(&list, EVERYONE, OverrideKind::Role),
        &role(EVERYONE, 0, PERM_CONNECT)
    );
}

#[test]
fn owner_extra_allow_is_clamped_and_never_escalates() {
    let input = RoomPermissionInput {
        owner_extra_allow: u64::MAX,
        ..base(false)
    };
    let list = overrides(plan_room_overrides(&input).unwrap());
    let owner = find(&list, OWNER, OverrideKind::Member);
    assert_eq!(owner.allow, OWNER_ALLOW_BITS | OWNER_EXTRA_MASK);
    // The documented guarantee, stated as bits: owning a room can never
    // grant Manage Roles or Administrator.
    assert_eq!(owner.allow & PERM_MANAGE_ROLES, 0);
    assert_eq!(owner.allow & PERM_ADMINISTRATOR, 0);
    // The exact documented owner bits: view/join/speak/stream/vad/priority
    // plus manage-channel/mute/deafen/move, and nothing else by default.
    assert_eq!(OWNER_ALLOW_BITS & PERM_MANAGE_ROLES, 0);
    assert_eq!(OWNER_ALLOW_BITS & PERM_ADMINISTRATOR, 0);
    assert_ne!(OWNER_ALLOW_BITS & PERM_MANAGE_CHANNELS, 0);
    assert_ne!(OWNER_ALLOW_BITS & PERM_MOVE_MEMBERS, 0);
    assert_ne!(OWNER_ALLOW_BITS & PERM_VIEW_CHANNEL, 0);
    assert_ne!(OWNER_ALLOW_BITS & PERM_CONNECT, 0);
    assert_ne!(OWNER_ALLOW_BITS & PERM_SPEAK, 0);
}

#[test]
fn source_deny_on_owner_wins_over_owner_grant() {
    // The source denies Speak to the owner: the grant must not override it.
    let source = [member(OWNER, PERM_SPEAK, PERM_SPEAK)];
    let input = RoomPermissionInput {
        source_overrides: &source,
        ..base(false)
    };
    let list = overrides(plan_room_overrides(&input).unwrap());
    let owner = find(&list, OWNER, OverrideKind::Member);
    assert_eq!(owner.allow & PERM_SPEAK, 0);
    assert_ne!(owner.deny & PERM_SPEAK, 0);
    assert_eq!(owner.allow & owner.deny, 0);
}

#[test]
fn required_role_gets_basic_access_in_private_rooms_only() {
    let source = [role(EVERYONE, PERM_VIEW_CHANNEL | PERM_CONNECT, 0)];
    let private = RoomPermissionInput {
        source_overrides: &source,
        required_role: Some(REQUIRED),
        ..base(true)
    };
    let list = overrides(plan_room_overrides(&private).unwrap());
    assert_eq!(
        find(&list, REQUIRED, OverrideKind::Role),
        &role(REQUIRED, REQUIRED_ROLE_ALLOW_BITS, 0)
    );

    let public = RoomPermissionInput {
        source_overrides: &source,
        required_role: Some(REQUIRED),
        ..base(false)
    };
    let list = overrides(plan_room_overrides(&public).unwrap());
    assert!(
        list.iter()
            .all(|o| !(o.id == REQUIRED && o.kind == OverrideKind::Role)),
        "public rooms grant nothing to the required role"
    );
}

#[test]
fn inheritance_source_variants_behave_identically() {
    let source = [role(EVERYONE, PERM_VIEW_CHANNEL, 0)];
    let plans: Vec<_> = [
        InheritanceSource::CreatorChannel,
        InheritanceSource::Category,
        InheritanceSource::ChosenChannel,
    ]
    .iter()
    .map(|s| {
        plan_room_overrides(&RoomPermissionInput {
            source: *s,
            source_overrides: &source,
            ..base(true)
        })
        .unwrap()
    })
    .collect();
    assert_eq!(plans[0], plans[1]);
    assert_eq!(plans[1], plans[2]);
}

#[test]
fn invalid_inputs_are_refused() {
    assert_eq!(
        plan_room_overrides(&RoomPermissionInput {
            owner_id: 0,
            ..base(false)
        }),
        Err(PermissionPlanError::InvalidId)
    );
    assert_eq!(
        plan_room_overrides(&RoomPermissionInput {
            everyone_role_id: 0,
            ..base(false)
        }),
        Err(PermissionPlanError::InvalidId)
    );
    assert_eq!(
        plan_room_overrides(&RoomPermissionInput {
            required_role: Some(EVERYONE),
            ..base(true)
        }),
        Err(PermissionPlanError::RequiredRoleIsEveryone)
    );
    let dup = [role(300, PERM_VIEW_CHANNEL, 0), role(300, PERM_CONNECT, 0)];
    assert_eq!(
        plan_room_overrides(&RoomPermissionInput {
            source_overrides: &dup,
            ..base(false)
        }),
        Err(PermissionPlanError::DuplicateSourceTarget)
    );
    // Same id with different kinds is fine: the target is (id, kind).
    let mixed = [
        role(300, PERM_VIEW_CHANNEL, 0),
        member(300, PERM_CONNECT, 0),
    ];
    assert!(plan_room_overrides(&RoomPermissionInput {
        source_overrides: &mixed,
        ..base(false)
    })
    .is_ok());
}

// ---- Property tests: the security contract ----

/// Grants the builder may add, per target: owner bits on the owner's member
/// target only, required-role bits on the required role target only.
fn granted_bits(
    id: u64,
    kind: OverrideKind,
    owner: u64,
    owner_extra: u64,
    required: Option<u64>,
) -> u64 {
    let mut granted = 0;
    if id == owner && kind == OverrideKind::Member {
        granted |= OWNER_ALLOW_BITS | (owner_extra & OWNER_EXTRA_MASK);
    }
    if Some(id) == required && kind == OverrideKind::Role {
        granted |= REQUIRED_ROLE_ALLOW_BITS;
    }
    granted
}

fn arb_override() -> impl Strategy<Value = (OverrideKind, u64, u64)> {
    (
        prop_oneof![Just(OverrideKind::Role), Just(OverrideKind::Member)],
        any::<u64>(),
        any::<u64>(),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_security_contract(
        raw in proptest::collection::vec(arb_override(), 0..8),
        owner_extra in any::<u64>(),
        private in any::<bool>(),
        use_required in any::<bool>(),
        owner_seed in proptest::option::of((any::<u64>(), any::<u64>())),
        everyone_seed in proptest::option::of((any::<u64>(), any::<u64>())),
        required_seed in proptest::option::of((any::<u64>(), any::<u64>())),
    ) {
        // Unique ids keep (id, kind) duplicate-free by construction; the
        // optional seeds collide with the owner/@everyone/required-role
        // targets on purpose to exercise the merge path (deny still wins).
        let mut source: Vec<ChannelOverride> = raw
            .into_iter()
            .enumerate()
            .map(|(i, (kind, allow, deny))| ChannelOverride {
                id: (i as u64) + 1,
                kind,
                allow,
                deny,
            })
            .collect();
        if let Some((allow, deny)) = owner_seed {
            source.push(member(OWNER, allow, deny));
        }
        if let Some((allow, deny)) = everyone_seed {
            source.push(role(EVERYONE, allow, deny));
        }
        let required = use_required.then_some(500u64);
        if let (Some(role_id), Some((allow, deny))) = (required, required_seed) {
            source.push(role(role_id, allow, deny));
        }
        let source_allow_of = |id: u64, kind: OverrideKind| -> u64 {
            source
                .iter()
                .find(|o| o.id == id && o.kind == kind)
                .map_or(0, |o| o.allow)
        };
        let required = use_required.then_some(500u64);
        let input = RoomPermissionInput {
            source: InheritanceSource::CreatorChannel,
            source_overrides: &source,
            bot_can_manage_roles: true,
            owner_id: OWNER,
            owner_extra_allow: owner_extra,
            private,
            everyone_role_id: EVERYONE,
            required_role: required,
        };

        let first = plan_room_overrides(&input).unwrap();
        let second = plan_room_overrides(&input).unwrap();
        prop_assert_eq!(&first, &second, "the builder is deterministic");
        let list = overrides(first);

        // (c) No duplicate targets.
        let mut seen = std::collections::HashSet::new();
        for o in &list {
            prop_assert!(seen.insert((o.id, o.kind)), "duplicate target {o:?}");
            prop_assert!(o.id != 0, "zero id in output");
            // (b) Deny wins over allow on every output target.
            prop_assert_eq!(o.allow & o.deny, 0, "allow/deny overlap on {o:?}");
            // (a) No granted bit the source did not allow, except the
            // documented grants on their own targets.
            let novel = o.allow & !source_allow_of(o.id, o.kind);
            let allowed_novel =
                granted_bits(o.id, o.kind, OWNER, owner_extra, required);
            prop_assert_eq!(
                novel & !allowed_novel,
                0,
                "output grants a bit the source did not have on {o:?}"
            );
        }

        // Owner entry always exists with the exact grant minus source denies.
        let owner_out = list
            .iter()
            .find(|o| o.id == OWNER && o.kind == OverrideKind::Member)
            .expect("owner entry must exist");
        let grant = OWNER_ALLOW_BITS | (owner_extra & OWNER_EXTRA_MASK);
        let owner_source = source
            .iter()
            .find(|o| o.id == OWNER && o.kind == OverrideKind::Member);
        let (src_allow, src_deny) =
            owner_source.map_or((0, 0), |o| (o.allow, o.deny));
        prop_assert_eq!(owner_out.allow, (src_allow | grant) & !src_deny);
        prop_assert_eq!(owner_out.allow & PERM_MANAGE_ROLES, 0);
        prop_assert_eq!(owner_out.allow & PERM_ADMINISTRATOR, 0);

        if private {
            let everyone = list
                .iter()
                .find(|o| o.id == EVERYONE && o.kind == OverrideKind::Role)
                .expect("@everyone entry must exist in private rooms");
            prop_assert_eq!(everyone.deny & PRIVATE_EVERYONE_DENY, PRIVATE_EVERYONE_DENY);
        }
    }
}
