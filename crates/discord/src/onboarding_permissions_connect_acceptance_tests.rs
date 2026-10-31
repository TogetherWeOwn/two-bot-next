//! Acceptance: a voice destination is only an actionable recommendation when
//! the member's effective permissions include both VIEW_CHANNEL and CONNECT.
//!
//! Protected acceptance file: the paired implementation must make the
//! Connect-denied cases pass without editing this file. The pinned seam is
//! `MemberAccess::permits_channel` with `post == false` on a voice channel
//! (Discord channel type 2): that is the exact check the session picker uses
//! before handing out a "Join voice now" link. Text/forum visibility with
//! `post == false`, and every `post == true` check, keep their current
//! meaning.

use super::*;
use serde_json::json;

const CONNECT: u64 = 1 << 20;
const VIEW_AND_SEND: u64 = VIEW_CHANNEL | SEND_MESSAGES; // 3072, as in the sibling fixtures
const VIEW_SEND_CONNECT: u64 = VIEW_AND_SEND | CONNECT;

fn access(everyone_permissions: u64, member_roles: &[&str]) -> MemberAccess {
    access_as(everyone_permissions, member_roles, "99")
}

fn access_as(everyone_permissions: u64, member_roles: &[&str], owner_id: &str) -> MemberAccess {
    MemberAccess::from_json(
        22,
        44,
        &json!({"id":"22","owner_id":owner_id}),
        &json!([
            {"id":"22","permissions":everyone_permissions.to_string()},
            {"id":"55","permissions":"0"}
        ]),
        &json!({"user":{"id":"44"},"roles":member_roles}),
    )
    .unwrap()
}

fn admin_access() -> MemberAccess {
    MemberAccess::from_json(
        22,
        44,
        &json!({"id":"22","owner_id":"99"}),
        &json!([
            {"id":"22","permissions":VIEW_AND_SEND.to_string()},
            {"id":"55","permissions":ADMINISTRATOR.to_string()}
        ]),
        &json!({"user":{"id":"44"},"roles":["55"]}),
    )
    .unwrap()
}

fn voice_channel(overwrites: serde_json::Value) -> serde_json::Value {
    json!({"id":"11","guild_id":"22","type":2,"permission_overwrites":overwrites})
}

fn overwrite(id: &str, kind: u64, allow: u64, deny: u64) -> serde_json::Value {
    json!({"id":id,"type":kind,"allow":allow.to_string(),"deny":deny.to_string()})
}

// --- RED at baseline: visible lobby, Connect missing or denied --------------

#[test]
fn voice_destination_without_connect_base_permission_is_not_recommended() {
    // Everyone can see the lobby (VIEW_CHANNEL) but nothing grants CONNECT.
    let channel = voice_channel(json!([]));
    assert_eq!(
        access(VIEW_AND_SEND, &[]).permits_channel(&channel, 11, false),
        Ok(false),
        "view alone must not make the voice lobby an actionable route"
    );
}

#[test]
fn voice_destination_everyone_connect_deny_is_not_recommended() {
    let channel = voice_channel(json!([overwrite("22", 0, 0, CONNECT)]));
    assert_eq!(
        access(VIEW_SEND_CONNECT, &[]).permits_channel(&channel, 11, false),
        Ok(false)
    );
}

#[test]
fn voice_destination_role_connect_deny_is_not_recommended() {
    let channel = voice_channel(json!([overwrite("55", 0, 0, CONNECT)]));
    assert_eq!(
        access(VIEW_SEND_CONNECT, &["55"]).permits_channel(&channel, 11, false),
        Ok(false)
    );
}

#[test]
fn voice_destination_member_connect_deny_wins_over_role_allow() {
    let channel = voice_channel(json!([
        overwrite("22", 0, 0, CONNECT),
        overwrite("55", 0, CONNECT, 0),
        overwrite("44", 1, 0, CONNECT)
    ]));
    assert_eq!(
        access(VIEW_SEND_CONNECT, &["55"]).permits_channel(&channel, 11, false),
        Ok(false),
        "the member overwrite is the final layer of the existing precedence"
    );
}

// --- Controls: must keep passing before and after the fix -------------------

#[test]
fn voice_destination_view_and_connect_routes() {
    let channel = voice_channel(json!([]));
    assert_eq!(
        access(VIEW_SEND_CONNECT, &[]).permits_channel(&channel, 11, false),
        Ok(true)
    );
}

#[test]
fn voice_destination_role_connect_allow_overrides_everyone_deny() {
    let channel = voice_channel(json!([
        overwrite("22", 0, 0, CONNECT),
        overwrite("55", 0, CONNECT, 0)
    ]));
    assert_eq!(
        access(VIEW_SEND_CONNECT, &["55"]).permits_channel(&channel, 11, false),
        Ok(true)
    );
}

#[test]
fn voice_destination_member_connect_allow_overrides_role_deny() {
    let channel = voice_channel(json!([
        overwrite("55", 0, 0, CONNECT),
        overwrite("44", 1, CONNECT, 0)
    ]));
    assert_eq!(
        access(VIEW_SEND_CONNECT, &["55"]).permits_channel(&channel, 11, false),
        Ok(true)
    );
}

#[test]
fn voice_destination_view_deny_blocks_despite_connect() {
    let channel = voice_channel(json!([overwrite("44", 1, 0, VIEW_CHANNEL)]));
    assert_eq!(
        access(VIEW_SEND_CONNECT, &[]).permits_channel(&channel, 11, false),
        Ok(false),
        "Connect never substitutes for View"
    );
}

#[test]
fn voice_destination_administrator_routes_despite_connect_deny() {
    let channel = voice_channel(json!([overwrite("44", 1, 0, VIEW_CHANNEL | CONNECT)]));
    assert_eq!(
        admin_access().permits_channel(&channel, 11, false),
        Ok(true),
        "administrator semantics of the existing calculator are preserved"
    );
}

#[test]
fn voice_destination_guild_owner_routes_despite_connect_deny() {
    let channel = voice_channel(json!([overwrite("44", 1, 0, VIEW_CHANNEL | CONNECT)]));
    assert_eq!(
        access_as(VIEW_AND_SEND, &[], "44").permits_channel(&channel, 11, false),
        Ok(true),
        "owner semantics of the existing calculator are preserved"
    );
}

#[test]
fn text_destination_visibility_still_needs_only_view() {
    // A Connect deny on a text channel is meaningless and must not hide it:
    // generic destination visibility (game hubs, text rooms) is unchanged.
    let channel = json!({"id":"10","guild_id":"22","type":0,"permission_overwrites":[overwrite("22", 0, 0, CONNECT)]});
    assert_eq!(
        access(VIEW_AND_SEND, &[]).permits_channel(&channel, 10, false),
        Ok(true)
    );
    assert_eq!(
        access(VIEW_AND_SEND, &[]).permits_channel(&channel, 10, true),
        Ok(true),
        "welcome posting checks (post) still require View+Send, not Connect"
    );
}

#[test]
fn forum_destination_visibility_still_needs_only_view() {
    let channel = json!({"id":"10","guild_id":"22","type":15,"permission_overwrites":[overwrite("22", 0, 0, CONNECT)]});
    assert_eq!(
        access(VIEW_AND_SEND, &[]).permits_channel(&channel, 10, false),
        Ok(true),
        "the game-hub forum stays a View-only visibility check"
    );
}

#[test]
fn connect_bit_follows_everyone_role_member_precedence() {
    // Calculator contract the fix must reuse, layer by layer.
    let mut channel = voice_channel(json!([overwrite("22", 0, 0, CONNECT)]));
    let bits = access(VIEW_SEND_CONNECT, &["55"])
        .channel_permissions(&channel)
        .unwrap()
        .unwrap();
    assert_eq!(bits & CONNECT, 0, "everyone deny removes Connect");
    channel["permission_overwrites"]
        .as_array_mut()
        .unwrap()
        .push(overwrite("55", 0, CONNECT, 0));
    let bits = access(VIEW_SEND_CONNECT, &["55"])
        .channel_permissions(&channel)
        .unwrap()
        .unwrap();
    assert_eq!(bits & CONNECT, CONNECT, "role allow re-grants Connect");
    channel["permission_overwrites"]
        .as_array_mut()
        .unwrap()
        .push(overwrite("44", 1, 0, CONNECT));
    let bits = access(VIEW_SEND_CONNECT, &["55"])
        .channel_permissions(&channel)
        .unwrap()
        .unwrap();
    assert_eq!(bits & CONNECT, 0, "member deny is final");
    assert_eq!(
        bits & VIEW_CHANNEL,
        VIEW_CHANNEL,
        "View survives throughout"
    );
}
