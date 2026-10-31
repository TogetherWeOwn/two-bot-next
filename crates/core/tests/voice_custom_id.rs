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
    // Wrong segment count, zero member, short/long/non-hex hashes. Hashes
    // are lowercase hex; uppercase is rejected.
    for id in [
        "two:voice:import-confirm:300",
        "two:voice:import-confirm:300:0123456789abcdef:extra",
        "two:voice:import-confirm:0:0123456789abcdef",
        "two:voice:import-cancel:0:0123456789abcdef",
        "two:voice:import-confirm:300:0123456789abcde",
        "two:voice:import-confirm:300:0123456789abcdef0",
        "two:voice:import-confirm:300:0123456789abcdeg",
        "two:voice:import-confirm:300:0123456789ABCDEF",
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

#[test]
fn hostile_name_ids_refuse_without_panic() {
    // Forged verbs, prefixes and lookalike separators: casing, whitespace,
    // underscores and foreign namespaces never decode.
    for id in [
        "two:voice:Name-Custom:500",
        "two:voice:NAME-CUSTOM:500",
        "two:voice:name-CUSTOM:500",
        "TWO:VOICE:name-custom:500",
        "Two:Voice:Name-Restore:500",
        "two:voice:name_custom:500",
        "two:voice:name custom:500",
        "two:voice:name-custom :500",
        "two:voice:name-custom:500 ",
        " two:voice:name-custom:500",
        "two:voice:name-custom:500\n",
        "two:voice:name-custom:500\r",
        "two:voice:name-custom:500\t",
        "two:lfg:name-custom:500",
        "two:self-role:name-custom:500",
        "two:voice:name-custom:500\u{200b}",
        "two:voice:name-restore:500\u{feff}",
        "\u{200b}two:voice:name-modal:500",
    ] {
        assert_eq!(parse_voice_custom_id(id), None, "{id:?}");
    }
    // Truncated shapes: missing verbs, missing rooms and cut-off tails.
    for id in [
        "two:voice:name-",
        "two:voice:name",
        "two:voice:name-custom",
        "two:voice:name-restore",
        "two:voice:name-modal",
        "two:voice:name-custom:",
        "two:voice:name-restore:",
        "two:voice:name-modal:",
        "two:voice:name-custo",
        "two:voice:",
        "two:voice",
        "",
    ] {
        assert_eq!(parse_voice_custom_id(id), None, "{id:?}");
    }
    // Separator confusion: extra segments, doubled colons and non-colon
    // separators never decode to a name action.
    for id in [
        "two:voice:name-custom:500:600",
        "two:voice:name-restore:500:600",
        "two:voice:name-modal:500:600",
        "two:voice:name-custom:500:",
        "two:voice:name-restore:500:",
        "two:voice:name-modal:500:",
        "two:voice:name-custom::500",
        "two:voice:name-custom:500::",
        "two:voice:name-custom:5:5:5",
        "two::voice:name-custom:500",
        "two:voice::name-custom:500",
        "::two:voice:name-custom:500",
        "two;voice;name-custom;500",
        "two:voice:name-custom;500",
        "two:voice:name-custom/500",
        "two:voice:name-custom|500",
        "two:voice:name-custom:50 0",
    ] {
        assert_eq!(parse_voice_custom_id(id), None, "{id:?}");
    }
    // Non-numeric rooms: signs, whitespace, hex, floats, overflow and zero.
    for id in [
        "two:voice:name-custom:0",
        "two:voice:name-restore:00",
        "two:voice:name-modal:000",
        "two:voice:name-custom:+500",
        "two:voice:name-custom:-500",
        "two:voice:name-custom: 500",
        "two:voice:name-custom:0x1F4",
        "two:voice:name-custom:1.5",
        "two:voice:name-custom:NaN",
        "two:voice:name-custom:abc",
        "two:voice:name-restore:abc",
        "two:voice:name-modal:abc",
        "two:voice:name-custom:18446744073709551616",
        "two:voice:name-restore:18446744073709551616",
        "two:voice:name-modal:99999999999999999999999",
    ] {
        assert_eq!(parse_voice_custom_id(id), None, "{id:?}");
    }
    // Control bytes, NUL and non-ASCII rooms never decode and never panic.
    for id in [
        "two:voice:name-custom:\u{0}",
        "two:voice:name-custom:500\u{0}",
        "two:voice:name-custom:500\u{7f}",
        "two:voice:name-restore:500\nmore",
        "two:voice:name-modal:500\r\n",
        "two:voice:name-custom:\u{e9}",
        "two:voice:name-custom:\u{1f600}",
        "two:voice:name-restore:\u{1f600}",
        "two:voice:name-modal:\u{200b}",
        "two:voice:name-custom:\u{ff15}\u{ff10}\u{ff10}",
        "two:voice:name-custom:caf\u{e9}",
    ] {
        assert_eq!(parse_voice_custom_id(id), None, "{id:?}");
    }
    // Overlong hostile runs stay rejections, never hangs.
    let padded = format!("two:voice:name-custom:{}", "1".repeat(90));
    assert!(padded.len() > MAX_VOICE_CUSTOM_ID_CHARS);
    assert_eq!(parse_voice_custom_id(&padded), None);
    assert_eq!(
        parse_voice_custom_id(&format!("two:voice:name-restore:{}", "a".repeat(512))),
        None
    );
    assert_eq!(
        parse_voice_custom_id(&format!("two:voice:name-modal:{}", "\u{1f600}".repeat(64))),
        None
    );
    // Determinism: hostile inputs parse the same way twice.
    for id in [
        "two:voice:name-custom:500:600",
        "two:voice:Name-Custom:500",
        "two:voice:name-custom:\u{0}",
    ] {
        assert_eq!(
            parse_voice_custom_id(id),
            parse_voice_custom_id(id),
            "{id:?}"
        );
    }
}

#[test]
fn name_exact_boundary_and_valid_controls_stay_green() {
    // Valid controls for every name verb, including the smallest room and
    // the largest encodable snowflake.
    for room_id in [1, 10, 500, u64::MAX] {
        assert_eq!(
            parse_voice_custom_id(&name_custom_custom_id(room_id)),
            Some(VoiceAction::NameCustom { room_id }),
            "{room_id}"
        );
        assert_eq!(
            parse_voice_custom_id(&name_restore_custom_id(room_id)),
            Some(VoiceAction::NameRestore { room_id }),
            "{room_id}"
        );
        assert_eq!(
            parse_voice_custom_id(&name_modal_custom_id(room_id)),
            Some(VoiceAction::NameModal { room_id }),
            "{room_id}"
        );
    }
    // The largest encodable name ids stay far inside Discord's limit.
    for id in [
        name_custom_custom_id(u64::MAX),
        name_restore_custom_id(u64::MAX),
        name_modal_custom_id(u64::MAX),
    ] {
        assert!(id.len() <= MAX_VOICE_CUSTOM_ID_CHARS, "{id}");
        assert!(parse_voice_custom_id(&id).is_some(), "{id}");
    }
    // Exact byte-length boundary for shape rejections: both 100- and
    // 101-byte all-`1` rooms overflow `u64`, so they refuse on shape at any
    // length. Lengths are asserted so the boundary cannot drift; the
    // zero-padded pair below pins the length gate itself.
    let custom_100 = format!("two:voice:name-custom:{}", "1".repeat(78));
    let custom_101 = format!("two:voice:name-custom:{}", "1".repeat(79));
    assert_eq!(custom_100.len(), MAX_VOICE_CUSTOM_ID_CHARS);
    assert_eq!(custom_101.len(), MAX_VOICE_CUSTOM_ID_CHARS + 1);
    assert_eq!(parse_voice_custom_id(&custom_100), None);
    assert_eq!(parse_voice_custom_id(&custom_101), None);
    let restore_100 = format!("two:voice:name-restore:{}", "1".repeat(77));
    let restore_101 = format!("two:voice:name-restore:{}", "1".repeat(78));
    assert_eq!(restore_100.len(), MAX_VOICE_CUSTOM_ID_CHARS);
    assert_eq!(restore_101.len(), MAX_VOICE_CUSTOM_ID_CHARS + 1);
    assert_eq!(parse_voice_custom_id(&restore_100), None);
    assert_eq!(parse_voice_custom_id(&restore_101), None);
    let modal_100 = format!("two:voice:name-modal:{}", "1".repeat(79));
    let modal_101 = format!("two:voice:name-modal:{}", "1".repeat(80));
    assert_eq!(modal_100.len(), MAX_VOICE_CUSTOM_ID_CHARS);
    assert_eq!(modal_101.len(), MAX_VOICE_CUSTOM_ID_CHARS + 1);
    assert_eq!(parse_voice_custom_id(&modal_100), None);
    assert_eq!(parse_voice_custom_id(&modal_101), None);
    // Zero-padded gate pin: `parse_id` accepts leading zeros, so the
    // exactly-100-byte room `000…0500` (value 500) parses while the 101-byte
    // form trips the length gate. Deleting the gate (or an off-by-one `>=`)
    // flips exactly one of these asserts.
    let zero_100 = format!("two:voice:name-custom:{}{}", "0".repeat(75), 500);
    let zero_101 = format!("two:voice:name-custom:{}{}", "0".repeat(76), 500);
    assert_eq!(zero_100.len(), MAX_VOICE_CUSTOM_ID_CHARS);
    assert_eq!(zero_101.len(), MAX_VOICE_CUSTOM_ID_CHARS + 1);
    assert_eq!(
        parse_voice_custom_id(&zero_100),
        Some(VoiceAction::NameCustom { room_id: 500 })
    );
    assert_eq!(parse_voice_custom_id(&zero_101), None);
    // Multibyte boundary: byte length (not chars or UTF-16 units) gates the
    // id. 39 `é` (78 bytes) plus the 22-byte custom prefix is exactly 100
    // bytes; 40 `é` is 102 bytes. Both refuse without panicking.
    let multi_100 = format!("two:voice:name-custom:{}", "é".repeat(39));
    let multi_over = format!("two:voice:name-custom:{}", "é".repeat(40));
    assert_eq!(multi_100.len(), MAX_VOICE_CUSTOM_ID_CHARS);
    assert!(multi_over.len() > MAX_VOICE_CUSTOM_ID_CHARS);
    assert_eq!(parse_voice_custom_id(&multi_100), None);
    assert_eq!(parse_voice_custom_id(&multi_over), None);
    // UTF-16 view: one emoji is one Rust char but two UTF-16 units and four
    // bytes. A short emoji room refuses on shape, a 64-emoji run refuses on
    // length (256 bytes of room plus the prefix).
    assert_eq!(
        parse_voice_custom_id("two:voice:name-custom:\u{1f600}"),
        None
    );
    let emoji_over = format!("two:voice:name-custom:{}", "\u{1f600}".repeat(25));
    assert!(emoji_over.len() > MAX_VOICE_CUSTOM_ID_CHARS);
    assert!(emoji_over.chars().count() < MAX_VOICE_CUSTOM_ID_CHARS);
    assert_eq!(parse_voice_custom_id(&emoji_over), None);
}
