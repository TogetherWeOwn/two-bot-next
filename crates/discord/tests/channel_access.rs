//! Channel-access resolution acceptance: pins the existing public API only.
//!
//! Covers `guild_permissions`, `resolve_channel_access` and `ChannelAccess`
//! from `crates/discord/src/channel_access.rs` (port of legacy
//! `src/discord/channelAccess.ts`, consumed by `crates/bot/src/preflight.rs`).
//! Twilight model types only, in-memory; no network, no token.

use twilight_model::{
    channel::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType},
    guild::{Permissions, Role},
    id::{marker::RoleMarker, Id},
};
use two_bot_discord::channel_access::{guild_permissions, resolve_channel_access, ChannelAccess};

const GUILD: u64 = 1;
const BOT: u64 = 4;

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

fn role_overwrite(id: u64, allow: Permissions, deny: Permissions) -> PermissionOverwrite {
    overwrite(id, PermissionOverwriteType::Role, allow, deny)
}

fn member_overwrite(id: u64, allow: Permissions, deny: Permissions) -> PermissionOverwrite {
    overwrite(id, PermissionOverwriteType::Member, allow, deny)
}

fn held(ids: &[u64]) -> Vec<Id<RoleMarker>> {
    ids.iter().map(|id| Id::new(*id)).collect()
}

fn access(role_ids: &[u64], roles: &[Role], overwrites: &[PermissionOverwrite]) -> ChannelAccess {
    resolve_channel_access(GUILD, BOT, &held(role_ids), roles, overwrites)
}

fn full() -> Permissions {
    Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::EMBED_LINKS
        | Permissions::MANAGE_MESSAGES
}

// (1) ADMINISTRATOR short-circuits to full access regardless of overwrites.
#[test]
fn administrator_short_circuits_channel_overwrites() {
    let none = Permissions::empty();
    let all = full();
    let roles = [role(GUILD, none), role(2, Permissions::ADMINISTRATOR)];
    // Deny everything at every overwrite stage, including @everyone.
    let overwrites = [
        role_overwrite(GUILD, none, all),
        role_overwrite(2, none, all),
        member_overwrite(BOT, none, all),
    ];
    let result = access(&[2], &roles, &overwrites);
    assert_eq!(
        result,
        ChannelAccess {
            view: true,
            send: true,
            embed: true,
            manage_messages: true,
            admin: true,
        }
    );
}

// Administrator carried by @everyone alone also short-circuits.
#[test]
fn administrator_from_everyone_role_short_circuits() {
    let roles = [role(GUILD, Permissions::ADMINISTRATOR)];
    let overwrites = [role_overwrite(
        GUILD,
        Permissions::empty(),
        Permissions::VIEW_CHANNEL,
    )];
    let result = access(&[], &roles, &overwrites);
    assert!(result.admin && result.view && result.send && result.embed && result.manage_messages);
}

// (2) Guild union covers @everyone + held roles only; unheld roles excluded.
#[test]
fn guild_union_covers_everyone_and_held_roles_only() {
    let roles = [
        role(GUILD, Permissions::VIEW_CHANNEL),
        role(2, Permissions::SEND_MESSAGES),
        role(9, Permissions::EMBED_LINKS | Permissions::MANAGE_MESSAGES),
    ];
    // Unheld role 9 contributes nothing.
    assert_eq!(
        guild_permissions(GUILD, &held(&[2]), &roles),
        Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES
    );
    // ... so its bits never surface in resolution either.
    let result = access(&[2], &roles, &[]);
    assert_eq!(
        result,
        ChannelAccess {
            view: true,
            send: true,
            embed: false,
            manage_messages: false,
            admin: false,
        }
    );
}

// (3) Overwrites apply in Discord order for view/send/embed/manage_messages.
#[test]
fn everyone_deny_removes_access_without_held_rescue() {
    let roles = [role(GUILD, full()), role(2, Permissions::empty())];
    let overwrites = [role_overwrite(
        GUILD,
        Permissions::empty(),
        Permissions::VIEW_CHANNEL,
    )];
    let result = access(&[2], &roles, &overwrites);
    assert!(!result.view && !result.send && !result.embed && !result.manage_messages);
}

#[test]
fn held_role_allow_rescues_everyone_deny() {
    let roles = [role(GUILD, full()), role(2, Permissions::empty())];
    let overwrites = [
        role_overwrite(GUILD, Permissions::empty(), Permissions::VIEW_CHANNEL),
        role_overwrite(2, Permissions::VIEW_CHANNEL, Permissions::empty()),
    ];
    let result = access(&[2], &roles, &overwrites);
    assert!(result.view && result.send && result.embed && result.manage_messages);
}

#[test]
fn role_allow_beats_sibling_role_deny() {
    let roles = [role(GUILD, full())];
    let overwrites = [
        role_overwrite(2, Permissions::empty(), Permissions::SEND_MESSAGES),
        role_overwrite(3, Permissions::SEND_MESSAGES, Permissions::empty()),
    ];
    let result = access(&[2, 3], &roles, &overwrites);
    assert!(result.view && result.send && result.embed && result.manage_messages);
}

#[test]
fn member_overwrite_decides_after_role_overwrites() {
    let roles = [role(GUILD, full())];
    let base = [
        role_overwrite(2, Permissions::empty(), Permissions::SEND_MESSAGES),
        role_overwrite(3, Permissions::SEND_MESSAGES, Permissions::empty()),
    ];
    // Member deny wins over the role-level allow: send (and therefore embed)
    // drop while view and manage_messages survive.
    let denied = access(
        &[2, 3],
        &roles,
        &[
            base[0],
            base[1],
            member_overwrite(BOT, Permissions::empty(), Permissions::SEND_MESSAGES),
        ],
    );
    assert_eq!(
        denied,
        ChannelAccess {
            view: true,
            send: false,
            embed: false,
            manage_messages: true,
            admin: false,
        }
    );
    // Member allow rescues the role-level deny.
    let allowed = access(
        &[2],
        &roles,
        &[
            role_overwrite(2, Permissions::empty(), Permissions::SEND_MESSAGES),
            member_overwrite(BOT, Permissions::SEND_MESSAGES, Permissions::empty()),
        ],
    );
    assert!(allowed.view && allowed.send && allowed.embed && allowed.manage_messages);
}

// Implicit gating: send needs view, embed needs send, manage needs view.
#[test]
fn capability_gating_follows_view_and_send() {
    let roles = [role(GUILD, full())];
    // Deny embed only: view + send + manage survive.
    let no_embed = access(
        &[2],
        &roles,
        &[role_overwrite(
            2,
            Permissions::empty(),
            Permissions::EMBED_LINKS,
        )],
    );
    assert_eq!(
        no_embed,
        ChannelAccess {
            view: true,
            send: true,
            embed: false,
            manage_messages: true,
            admin: false,
        }
    );
    // Deny manage only: view + send + embed survive.
    let no_manage = access(
        &[],
        &roles,
        &[member_overwrite(
            BOT,
            Permissions::empty(),
            Permissions::MANAGE_MESSAGES,
        )],
    );
    assert_eq!(
        no_manage,
        ChannelAccess {
            view: true,
            send: true,
            embed: true,
            manage_messages: false,
            admin: false,
        }
    );
    // Send without view is not usable.
    let send_only = [role(GUILD, Permissions::SEND_MESSAGES)];
    let result = access(&[], &send_only, &[]);
    assert_eq!(
        result,
        ChannelAccess {
            view: false,
            send: false,
            embed: false,
            manage_messages: false,
            admin: false,
        }
    );
}

// (4) Overwrites for another channel/member do not leak into the result.
#[test]
fn unheld_role_and_foreign_member_overwrites_do_not_leak() {
    let roles = [role(GUILD, full()), role(2, Permissions::empty())];
    // Role overwrite for unheld role 9 and member overwrite for another
    // member (BOT + 1) must not change the outcome.
    let overwrites = [
        role_overwrite(9, Permissions::empty(), Permissions::VIEW_CHANNEL),
        member_overwrite(BOT + 1, Permissions::empty(), full()),
    ];
    let result = access(&[2], &roles, &overwrites);
    assert!(result.view && result.send && result.embed && result.manage_messages);
}

#[test]
fn sibling_channel_overwrites_resolve_independently() {
    let roles = [role(GUILD, full()), role(2, Permissions::empty())];
    let dark = [role_overwrite(
        GUILD,
        Permissions::empty(),
        Permissions::VIEW_CHANNEL,
    )];
    let lit = [
        dark[0],
        role_overwrite(2, Permissions::VIEW_CHANNEL, Permissions::empty()),
    ];
    // Each channel resolves from its own overwrite slice only: the lit
    // channel's held-role allow never leaks into the dark channel.
    assert!(!access(&[2], &roles, &dark).view);
    assert!(access(&[2], &roles, &lit).view);
}
