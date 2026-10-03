//! Acceptance cases for the pure `two:voice:` component-id codec.

use proptest::prelude::*;
use two_bot_core::voice_custom_id::{
    import_cancel_custom_id, import_confirm_custom_id, join_custom_id, kick_custom_id,
    name_custom_custom_id, name_modal_custom_id, name_restore_custom_id, parse_voice_custom_id,
    VoiceAction, IMPORT_HASH_CHARS, MAX_VOICE_CUSTOM_ID_CHARS, VOICE_CUSTOM_ID_PREFIX,
};
use two_bot_core::voice_private::JoinDecision;
use two_bot_core::voice_vote_kick::VoteBallot;

fn decisions() -> impl Strategy<Value = JoinDecision> {
    prop_oneof![
        Just(JoinDecision::Approve),
        Just(JoinDecision::Deny),
        Just(JoinDecision::Block),
    ]
}

fn ballots() -> impl Strategy<Value = VoteBallot> {
    prop_oneof![Just(VoteBallot::Yes), Just(VoteBallot::No)]
}

fn nonzero_id() -> impl Strategy<Value = u64> {
    1..=u64::MAX
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Every encoded id round-trips through the parser.
    #[test]
    fn round_trip_join(
        decision in decisions(),
        room_id in nonzero_id(),
        request_id in nonzero_id(),
    ) {
        let expected = match decision {
            JoinDecision::Approve => VoiceAction::JoinApprove { room_id, request_id },
            JoinDecision::Deny => VoiceAction::JoinDeny { room_id, request_id },
            JoinDecision::Block => VoiceAction::JoinBlock { room_id, request_id },
        };
        prop_assert_eq!(
            parse_voice_custom_id(&join_custom_id(decision, room_id, request_id)),
            Some(expected)
        );
    }

    /// Every encoded id round-trips through the parser.
    #[test]
    fn round_trip_kick(ballot in ballots(), vote_id in nonzero_id()) {
        let expected = match ballot {
            VoteBallot::Yes => VoiceAction::KickYes { vote_id },
            VoteBallot::No => VoiceAction::KickNo { vote_id },
        };
        prop_assert_eq!(
            parse_voice_custom_id(&kick_custom_id(ballot, vote_id)),
            Some(expected)
        );
    }

    /// Every encoded id round-trips through the parser.
    #[test]
    fn round_trip_name(room_id in nonzero_id()) {
        prop_assert_eq!(
            parse_voice_custom_id(&name_custom_custom_id(room_id)),
            Some(VoiceAction::NameCustom { room_id })
        );
        prop_assert_eq!(
            parse_voice_custom_id(&name_restore_custom_id(room_id)),
            Some(VoiceAction::NameRestore { room_id })
        );
        prop_assert_eq!(
            parse_voice_custom_id(&name_modal_custom_id(room_id)),
            Some(VoiceAction::NameModal { room_id })
        );
    }

    /// The largest encodable ids (20-digit snowflakes, `u64::MAX` requests)
    /// stay within Discord's 100-character component-id limit.
    #[test]
    fn every_encoded_id_fits_limit(
        room_id in nonzero_id(),
        request_id in nonzero_id(),
    ) {
        for id in [
            join_custom_id(JoinDecision::Approve, room_id, request_id),
            join_custom_id(JoinDecision::Deny, room_id, request_id),
            join_custom_id(JoinDecision::Block, room_id, request_id),
            kick_custom_id(VoteBallot::Yes, room_id),
            kick_custom_id(VoteBallot::No, room_id),
            name_custom_custom_id(room_id),
            name_restore_custom_id(room_id),
            name_modal_custom_id(room_id),
        ] {
            prop_assert!(id.len() <= MAX_VOICE_CUSTOM_ID_CHARS, "{id}");
        }
    }
}

#[test]
fn golden_shapes_carry_room_request_and_vote() {
    assert_eq!(
        join_custom_id(JoinDecision::Approve, 10, 1),
        "two:voice:join-approve:10:1"
    );
    assert_eq!(
        join_custom_id(JoinDecision::Deny, 10, 2),
        "two:voice:join-deny:10:2"
    );
    assert_eq!(
        join_custom_id(JoinDecision::Block, 10, 3),
        "two:voice:join-block:10:3"
    );
    assert_eq!(kick_custom_id(VoteBallot::Yes, 99), "two:voice:kick-yes:99");
    assert_eq!(kick_custom_id(VoteBallot::No, 99), "two:voice:kick-no:99");
    assert_eq!(name_custom_custom_id(10), "two:voice:name-custom:10");
    assert_eq!(name_restore_custom_id(10), "two:voice:name-restore:10");
    assert_eq!(name_modal_custom_id(10), "two:voice:name-modal:10");
}

#[test]
fn rejects_garbage_overlong_and_non_numeric() {
    // Garbage: wrong prefix, unknown verbs, missing or extra segments.
    for id in [
        "",
        "two:voice:",
        "two:voice:join-approve",
        "two:voice:join-approve:10",
        "two:voice:join-approve:10:1:extra",
        "two:voice:join-approve::1",
        "two:voice:kick-yes",
        "two:voice:kick-yes:9:9",
        "two:voice:name-custom",
        "two:voice:name-custom:1:2",
        "two:voice:explode:10",
        "two:lfg:10",
        "two:self-role:games",
        "two:self-role:games:chess",
        "plain-text",
    ] {
        assert_eq!(parse_voice_custom_id(id), None, "{id}");
    }
    // Overlong: a valid shape padded past the 100-char limit.
    let long = format!("two:voice:name-custom:{}", "1".repeat(90));
    assert!(long.len() > MAX_VOICE_CUSTOM_ID_CHARS);
    assert_eq!(parse_voice_custom_id(&long), None);
    // Non-numeric: signs, whitespace, hex, floats and overflow.
    for id in [
        "two:voice:join-approve:+10:1",
        "two:voice:join-approve: 10:1",
        "two:voice:join-approve:0x10:1",
        "two:voice:join-approve:1.5:1",
        "two:voice:join-approve:18446744073709551616:1",
        "two:voice:kick-yes:-1",
        "two:voice:kick-yes:NaN",
        "two:voice:name-modal:abc",
    ] {
        assert_eq!(parse_voice_custom_id(id), None, "{id}");
    }
    // Zero ids: no voice id is ever zero.
    for id in [
        "two:voice:join-approve:0:1",
        "two:voice:join-approve:10:0",
        "two:voice:kick-yes:0",
        "two:voice:name-custom:0",
        "two:voice:name-restore:0",
        "two:voice:name-modal:0",
    ] {
        assert_eq!(parse_voice_custom_id(id), None, "{id}");
    }
}

fn content_hash() -> impl Strategy<Value = String> {
    (0u64..=u64::MAX).prop_map(|hash| format!("{hash:016x}"))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Import confirm/cancel ids round-trip, stay in the voice namespace
    /// and fit Discord's 100-character limit even for 20-digit snowflakes.
    #[test]
    fn round_trip_import(member_id in nonzero_id(), hash in content_hash()) {
        prop_assert_eq!(hash.len(), IMPORT_HASH_CHARS);
        prop_assert_eq!(
            parse_voice_custom_id(&import_confirm_custom_id(member_id, &hash)),
            Some(VoiceAction::ImportConfirm {
                member_id,
                hash: hash.clone(),
            })
        );
        prop_assert_eq!(
            parse_voice_custom_id(&import_cancel_custom_id(member_id, &hash)),
            Some(VoiceAction::ImportCancel {
                member_id,
                hash,
            })
        );
        prop_assert!(
            import_confirm_custom_id(u64::MAX, &"f".repeat(IMPORT_HASH_CHARS)).len()
                <= MAX_VOICE_CUSTOM_ID_CHARS
        );
    }
}

#[test]
fn golden_import_shapes_bind_member_and_hash() {
    assert_eq!(
        import_confirm_custom_id(300, "0123456789abcdef"),
        "two:voice:import-confirm:300:0123456789abcdef"
    );
    assert_eq!(
        import_cancel_custom_id(300, "0123456789abcdef"),
        "two:voice:import-cancel:300:0123456789abcdef"
    );
}

#[test]
fn rejects_malformed_import_hashes() {
    // Wrong segment count, zero member, short/long/non-hex hashes.
    for id in [
        "two:voice:import-confirm:300",
        "two:voice:import-confirm:300:0123456789abcdef:extra",
        "two:voice:import-confirm:0:0123456789abcdef",
        "two:voice:import-cancel:0:0123456789abcdef",
        "two:voice:import-confirm:300:0123456789abcde",
        "two:voice:import-confirm:300:0123456789abcdef0",
        "two:voice:import-confirm:300:0123456789abcdeg",
        "two:voice:import-confirm:300:----------------",
        "two:voice:import-confirm:300:",
        "two:voice:import-cancel:300:0123456789abcde ",
    ] {
        assert_eq!(parse_voice_custom_id(id), None, "{id}");
    }
}

#[test]
fn no_collision_with_lfg_or_self_role_namespaces() {
    for id in ["two:lfg:abc123", "two:self-role:games:valorant"] {
        assert!(
            !id.starts_with(VOICE_CUSTOM_ID_PREFIX),
            "voice prefix must not match {id}"
        );
        assert_eq!(parse_voice_custom_id(id), None);
    }
    // The voice prefix does not match the other namespaces either.
    assert!(!"two:voice:kick-yes:1".starts_with("two:lfg:"));
    assert!(!"two:voice:kick-yes:1".starts_with("two:self-role:"));
}
