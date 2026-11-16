use super::*;
use crate::voice_permissions::PERM_CONNECT;

#[test]
fn permission_order_follows_discord_not_array_order() {
    let roles = [
        GuildRoleFact {
            id: 1,
            position: 0,
            permissions: PERM_CONNECT,
        },
        GuildRoleFact {
            id: 3,
            position: 1,
            permissions: 0,
        },
        GuildRoleFact {
            id: 4,
            position: 2,
            permissions: 0,
        },
    ];
    let overwrites = [
        ChannelOverride {
            id: 5,
            kind: OverrideKind::Member,
            allow: 0,
            deny: PERM_CONNECT,
        },
        ChannelOverride {
            id: 4,
            kind: OverrideKind::Role,
            allow: PERM_CONNECT,
            deny: 0,
        },
        ChannelOverride {
            id: 3,
            kind: OverrideKind::Role,
            allow: 0,
            deny: PERM_CONNECT,
        },
        ChannelOverride {
            id: 1,
            kind: OverrideKind::Role,
            allow: 0,
            deny: PERM_CONNECT,
        },
    ];
    assert_eq!(
        effective_perms(&roles, &overwrites, 1, 2, 5, &[1, 3, 4]),
        Some(0)
    );
    assert_eq!(
        effective_perms(&roles, &overwrites, 1, 2, 6, &[3, 4]),
        Some(PERM_CONNECT)
    );
    assert_eq!(effective_perms(&roles, &overwrites, 1, 2, 6, &[3]), Some(0));
}

#[test]
fn missing_facts_fail_closed_and_admin_and_owner_bypass() {
    let roles = [
        GuildRoleFact {
            id: 1,
            position: 0,
            permissions: 0,
        },
        GuildRoleFact {
            id: 3,
            position: 1,
            permissions: PERM_ADMINISTRATOR,
        },
    ];
    assert_eq!(effective_perms(&[], &[], 1, 2, 5, &[]), None);
    assert_eq!(effective_perms(&roles, &[], 1, 2, 5, &[99]), None);
    assert_eq!(effective_perms(&roles, &[], 1, 2, 5, &[3, 99]), None);
    assert_eq!(effective_perms(&roles, &[], 1, 2, 5, &[3]), Some(u64::MAX));
    assert_eq!(effective_perms(&[], &[], 1, 2, 2, &[99]), Some(u64::MAX));
}
