//! Pure REST permission resolution, ported from legacy `src/discord/channelAccess.ts`.
//! Source: https://docs.discord.com/developers/topics/permissions#permission-overwrites

use twilight_model::{
    channel::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    guild::{Permissions, Role},
    id::{marker::RoleMarker, Id},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelAccess {
    pub view: bool,
    pub send: bool,
    pub embed: bool,
    pub manage_messages: bool,
    pub admin: bool,
}

pub fn guild_permissions(guild_id: u64, held: &[Id<RoleMarker>], roles: &[Role]) -> Permissions {
    roles
        .iter()
        .filter(|role| role.id.get() == guild_id || held.contains(&role.id))
        .fold(Permissions::empty(), |permissions, role| {
            permissions | role.permissions
        })
}

pub fn resolve_channel_access(
    guild_id: u64,
    bot_id: u64,
    held: &[Id<RoleMarker>],
    roles: &[Role],
    overwrites: &[PermissionOverwrite],
) -> ChannelAccess {
    let mut permissions = guild_permissions(guild_id, held, roles);
    let admin = permissions.contains(Permissions::ADMINISTRATOR);
    if admin {
        return ChannelAccess {
            view: true,
            send: true,
            embed: true,
            manage_messages: true,
            admin: true,
        };
    }

    if let Some(everyone) = overwrites.iter().find(|overwrite| {
        overwrite.kind == PermissionOverwriteType::Role && overwrite.id.get() == guild_id
    }) {
        permissions = (permissions & !everyone.deny) | everyone.allow;
    }

    let mut allow = Permissions::empty();
    let mut deny = Permissions::empty();
    for overwrite in overwrites.iter().filter(|overwrite| {
        overwrite.kind == PermissionOverwriteType::Role
            && overwrite.id.get() != guild_id
            && held.iter().any(|role| role.get() == overwrite.id.get())
    }) {
        allow |= overwrite.allow;
        deny |= overwrite.deny;
    }
    permissions = (permissions & !deny) | allow;

    if let Some(member) = overwrites.iter().find(|overwrite| {
        overwrite.kind == PermissionOverwriteType::Member && overwrite.id.get() == bot_id
    }) {
        permissions = (permissions & !member.deny) | member.allow;
    }

    let view = permissions.contains(Permissions::VIEW_CHANNEL);
    let send = view && permissions.contains(Permissions::SEND_MESSAGES);
    ChannelAccess {
        view,
        send,
        // Discord implicitly denies embeds without Send, and all access without View.
        embed: send && permissions.contains(Permissions::EMBED_LINKS),
        manage_messages: view && permissions.contains(Permissions::MANAGE_MESSAGES),
        admin,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(id: u64, permissions: Permissions) -> Role {
        serde_json::from_value(serde_json::json!({
            "id": id.to_string(), "name": "fixture", "color": 0, "colors": {"primary_color": 0}, "hoist": false,
            "position": 1, "permissions": permissions.bits().to_string(),
            "managed": false, "mentionable": false, "flags": 0
        }))
        .unwrap()
    }

    fn overwrite(
        id: u64,
        kind: PermissionOverwriteType,
        allow: Permissions,
        deny: Permissions,
    ) -> PermissionOverwrite {
        PermissionOverwrite {
            id: Id::new(id),
            kind,
            allow,
            deny,
        }
    }

    #[test]
    fn legacy_channel_access_table() {
        let none = Permissions::empty();
        let view = Permissions::VIEW_CHANNEL;
        let send = Permissions::SEND_MESSAGES;
        let all = view | send | Permissions::EMBED_LINKS | Permissions::MANAGE_MESSAGES;
        let r = PermissionOverwriteType::Role;
        let m = PermissionOverwriteType::Member;
        let cases = [
            (
                "plain everyone",
                all,
                none,
                vec![],
                (true, true, true, true, false),
            ),
            (
                "administrator bypass",
                none,
                Permissions::ADMINISTRATOR,
                vec![overwrite(1, r, none, all)],
                (true, true, true, true, true),
            ),
            (
                "send without view",
                send,
                none,
                vec![],
                (false, false, false, false, false),
            ),
            (
                "everyone deny",
                all,
                none,
                vec![overwrite(1, r, none, view)],
                (false, false, false, false, false),
            ),
            (
                "unheld staff allow cannot rescue",
                all,
                none,
                vec![overwrite(1, r, none, view), overwrite(9, r, view, none)],
                (false, false, false, false, false),
            ),
            (
                "held allow rescues staff-alert shape",
                all,
                none,
                vec![overwrite(1, r, none, view), overwrite(2, r, view, none)],
                (true, true, true, true, false),
            ),
            (
                "role allow beats another deny",
                all,
                none,
                vec![overwrite(2, r, none, send), overwrite(3, r, send, none)],
                (true, true, true, true, false),
            ),
            (
                "member deny wins",
                all,
                none,
                vec![
                    overwrite(2, r, none, send),
                    overwrite(3, r, send, none),
                    overwrite(4, m, none, send),
                ],
                (true, false, false, true, false),
            ),
            (
                "member allow wins",
                all,
                none,
                vec![overwrite(2, r, none, send), overwrite(4, m, send, none)],
                (true, true, true, true, false),
            ),
            (
                "embed denied",
                all,
                none,
                vec![overwrite(2, r, none, Permissions::EMBED_LINKS)],
                (true, true, false, true, false),
            ),
            (
                "manage messages denied",
                all,
                none,
                vec![overwrite(4, m, none, Permissions::MANAGE_MESSAGES)],
                (true, true, true, false, false),
            ),
        ];
        for (name, everyone, bot, overwrites, expected) in cases {
            let result = resolve_channel_access(
                1,
                4,
                &[Id::new(2), Id::new(3)],
                &[role(1, everyone), role(2, bot), role(3, none)],
                &overwrites,
            );
            assert_eq!(
                (
                    result.view,
                    result.send,
                    result.embed,
                    result.manage_messages,
                    result.admin
                ),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn legacy_staff_alert_permission_values() {
        let admin = Permissions::ADMINISTRATOR;
        let owen = Permissions::from_bits_truncate(8_866_461_766_385_663) & !admin;
        let roles = [
            role(1, Permissions::from_bits_truncate(2_111_339_353_935_424)),
            role(2, owen),
            role(9, Permissions::from_bits_truncate(32_957_699_452_871)),
        ];
        assert!(owen.contains(Permissions::SEND_MESSAGES));
        let result = resolve_channel_access(
            1,
            4,
            &[Id::new(2)],
            &roles,
            &[
                overwrite(
                    1,
                    PermissionOverwriteType::Role,
                    Permissions::empty(),
                    Permissions::VIEW_CHANNEL,
                ),
                overwrite(
                    9,
                    PermissionOverwriteType::Role,
                    Permissions::VIEW_CHANNEL,
                    Permissions::empty(),
                ),
            ],
        );
        assert!(!result.view && !result.send && !result.admin);
    }

    #[test]
    fn game_categories_do_not_grant_access_to_unsynced_children() {
        let none = Permissions::empty();
        let view = Permissions::VIEW_CHANNEL;
        let roles = [role(1, none), role(2, none)];
        let dark = [overwrite(1, PermissionOverwriteType::Role, none, view)];
        let lit = [
            dark[0],
            overwrite(2, PermissionOverwriteType::Role, view, none),
        ];
        // Three categories and three children: resolve each object's own overwrites.
        for (name, category, child, visible) in [
            ("dark", &dark[..], &dark[..], 0),
            ("lit", &lit[..], &lit[..], 6),
            ("categories-only", &lit[..], &dark[..], 3),
        ] {
            let count = (0..3)
                .flat_map(|_| [category, child])
                .filter(|overwrites| {
                    resolve_channel_access(1, 4, &[Id::new(2)], &roles, overwrites).view
                })
                .count();
            assert_eq!(count, visible, "{name}");
        }
    }
}
