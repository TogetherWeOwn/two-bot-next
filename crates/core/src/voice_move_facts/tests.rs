use super::*;
use crate::voice_permissions::{PERM_CONNECT, PERM_VIEW_CHANNEL};

#[test]
fn override_parsing_rejects_shapes_it_cannot_prove() {
    let role = serde_json::json!({"id": "20", "type": 0, "allow": "1048576", "deny": "0"});
    assert_eq!(
        parse_override(&role),
        Some(ChannelOverride {
            id: 20,
            kind: OverrideKind::Role,
            allow: PERM_CONNECT,
            deny: 0,
        })
    );
    let member = serde_json::json!({"id": "7", "type": 1, "allow": "0", "deny": "1024"});
    assert_eq!(
        parse_override(&member),
        Some(ChannelOverride {
            id: 7,
            kind: OverrideKind::Member,
            allow: 0,
            deny: PERM_VIEW_CHANNEL,
        })
    );
    for bad in [
        serde_json::json!(null),
        serde_json::json!({"id": "0", "type": 0, "allow": "0", "deny": "0"}),
        serde_json::json!({"id": "20", "type": 7, "allow": "0", "deny": "0"}),
        serde_json::json!({"id": "nope", "type": 0, "allow": "0", "deny": "0"}),
        serde_json::json!({"id": "20", "type": 1, "allow": "1048576"}),
        serde_json::json!({"id": "20", "type": 0, "allow": "0x10", "deny": "0"}),
        serde_json::json!({"id": 20, "type": 0, "allow": "0", "deny": "0"}),
        serde_json::json!({"id": "20", "type": "0", "allow": "0", "deny": "0"}),
        serde_json::json!({"id": "20", "type": 0, "allow": 0, "deny": "0"}),
        serde_json::json!({"id": "20", "type": 0, "allow": "", "deny": "0"}),
        serde_json::json!({"id": "+20", "type": 0, "allow": "0", "deny": "0"}),
        serde_json::json!({"id": "20", "type": 0, "allow": (u128::from(u64::MAX) + 1).to_string(), "deny": "0"}),
        serde_json::json!({"id": "20", "type": 0, "allow": "0", "deny": "-1"}),
    ] {
        assert_eq!(parse_override(&bad), None, "{bad}");
    }
}

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
fn channel_overwrites_cannot_grant_administrator() {
    let roles = [
        GuildRoleFact {
            id: 1,
            position: 0,
            permissions: 0,
        },
        GuildRoleFact {
            id: 3,
            position: 1,
            permissions: 0,
        },
    ];
    for (id, kind) in [
        (1, OverrideKind::Role),
        (3, OverrideKind::Role),
        (5, OverrideKind::Member),
    ] {
        let overwrite = ChannelOverride {
            id,
            kind,
            allow: PERM_ADMINISTRATOR | PERM_CONNECT,
            deny: 0,
        };
        assert_eq!(
            effective_perms(&roles, &[overwrite], 1, 2, 5, &[3]),
            Some(PERM_CONNECT)
        );
    }
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
