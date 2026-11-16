use super::*;
use crate::voice_permissions::OverrideKind;

fn facts() -> VoiceMoveFacts {
    let room = |id| ChannelFact {
        id,
        is_voice: true,
        is_afk: false,
        user_limit: 0,
        overwrites: vec![],
    };
    VoiceMoveFacts {
        guild_id: 1,
        guild_owner_id: 2,
        guild_roles: vec![GuildRoleFact {
            id: 1,
            position: 0,
            permissions: BOT_ROOM_BITS,
        }],
        target: room(4),
        source: room(3),
        member_channel_id: Some(3),
        target_occupants: 0,
        member_id: 5,
        member_roles: vec![],
        member_timed_out: false,
        bot_id: 6,
        bot_roles: vec![],
    }
}

fn refused(f: &VoiceMoveFacts, outcome: VoiceMoveOutcome) {
    assert_eq!(decide(f), VoiceMoveDecision::Refuse(outcome));
}

#[test]
fn decision_order_and_capacity() {
    let mut f = facts();
    assert_eq!(
        decide(&f),
        VoiceMoveDecision::Proceed { from_channel_id: 3 }
    );
    f.guild_roles.clear();
    f.member_channel_id = Some(4);
    refused(&f, VoiceMoveOutcome::AlreadyInRoom);
    f.member_channel_id = None;
    refused(&f, VoiceMoveOutcome::NotConnected);
    f.target.is_afk = true;
    refused(&f, VoiceMoveOutcome::RoomGone);
    f.target.is_afk = false;
    f.target.is_voice = false;
    refused(&f, VoiceMoveOutcome::RoomGone);
    f = facts();
    f.target.user_limit = 1;
    f.target_occupants = 1;
    f.bot_roles.push(99);
    refused(&f, VoiceMoveOutcome::RoomFull);
    f.member_timed_out = true;
    refused(&f, VoiceMoveOutcome::NotPermitted);
}

#[test]
fn unknown_roles_fail_closed_and_both_bot_rooms_are_checked() {
    let mut f = facts();
    f.member_roles.push(99);
    assert_eq!(decide(&f), VoiceMoveDecision::Incomplete);
    f = facts();
    f.bot_roles.push(99);
    assert_eq!(decide(&f), VoiceMoveDecision::Incomplete);
    f = facts();
    f.guild_roles.clear();
    assert_eq!(decide(&f), VoiceMoveDecision::Incomplete);
    for source in [true, false] {
        f = facts();
        let room = if source { &mut f.source } else { &mut f.target };
        room.overwrites.push(ChannelOverride {
            id: f.bot_id,
            kind: OverrideKind::Member,
            allow: 0,
            deny: PERM_MOVE_MEMBERS,
        });
        refused(&f, VoiceMoveOutcome::BotNotPermitted);
    }
}

#[test]
fn member_permissions_and_timeout_are_fail_closed() {
    let mut f = facts();
    f.target.overwrites.push(ChannelOverride {
        id: f.member_id,
        kind: OverrideKind::Member,
        allow: PERM_ADMINISTRATOR,
        deny: PERM_CONNECT,
    });
    refused(&f, VoiceMoveOutcome::NotPermitted);
    f.target.overwrites.clear();
    f.member_timed_out = true;
    refused(&f, VoiceMoveOutcome::NotPermitted);
    f.member_id = f.guild_owner_id;
    assert!(matches!(decide(&f), VoiceMoveDecision::Proceed { .. }));
    f.member_id = 5;
    f.guild_roles[0].permissions |= PERM_ADMINISTRATOR;
    assert!(matches!(decide(&f), VoiceMoveDecision::Proceed { .. }));
}
