//! Fail-open acceptance: an absent voice toggle must never enable by default.
//!
//! Protected failing-first tests (paired build card implements the fix; the
//! Builder must not edit this file). The `/textchannels`, `/group` and
//! `/alwaysprivate` decisions currently fold `request.enabled.unwrap_or(true)`
//! into the creator row, so absent input silently switches the feature on.
//! The contract pinned here, per command:
//!
//! - `enabled: None` on a stored-off creator either refuses (any non-empty,
//!   deterministic message) or preserves the stored value exactly. The
//!   Builder picks one per command and pins it; silently writing `true`
//!   is the only outcome that fails.
//! - `enabled: Some(_)` sets the stored value exactly as today.
//! - An unknown creator still refuses (healthy path unchanged).
//!
//! Pure decisions only: no database, no Discord, no environment.

use two_bot::voice_rooms::{
    decide_always_private, decide_group, decide_text_channels, AlwaysPrivatePlan,
    AlwaysPrivateRequest, GroupPlan, GroupRequest, TextChannelsPlan, TextChannelsRequest,
};
use two_bot_core::voice_rooms::CreatorChannel;

fn creator() -> CreatorChannel {
    // Spec defaults store every toggle off.
    CreatorChannel::new(11, 22)
}

#[test]
fn textchannels_absent_toggle_does_not_enable_by_default() {
    let request = TextChannelsRequest {
        enabled: None,
        name: None,
        viewer_role: None,
    };
    match decide_text_channels(Some(creator()), &request) {
        TextChannelsPlan::Refuse { message } => {
            assert!(!message.is_empty(), "a refusal carries fixed vocabulary");
        }
        TextChannelsPlan::Update(next) => {
            assert!(
                !next.text_channels,
                "absent toggle preserves stored off, never enables"
            );
        }
    }
}

#[test]
fn textchannels_named_request_without_toggle_does_not_enable() {
    // Naming a companion channel without an explicit toggle must not smuggle
    // the feature on either: refuse the request or keep stored off.
    let request = TextChannelsRequest {
        enabled: None,
        name: Some("lounge".to_owned()),
        viewer_role: None,
    };
    match decide_text_channels(Some(creator()), &request) {
        TextChannelsPlan::Refuse { message } => {
            assert!(!message.is_empty(), "a refusal carries fixed vocabulary");
        }
        TextChannelsPlan::Update(next) => {
            assert!(
                !next.text_channels,
                "a named request without a toggle preserves stored off"
            );
        }
    }
}

#[test]
fn group_absent_toggle_does_not_enable_by_default() {
    let request = GroupRequest { enabled: None };
    match decide_group(Some(creator()), &request) {
        GroupPlan::Refuse { message } => {
            assert!(!message.is_empty(), "a refusal carries fixed vocabulary");
        }
        GroupPlan::Update(next) => {
            assert!(
                !next.group_by_category,
                "absent toggle preserves stored off, never enables"
            );
        }
    }
}

#[test]
fn always_private_absent_toggle_does_not_enable_by_default() {
    let request = AlwaysPrivateRequest { enabled: None };
    match decide_always_private(Some(creator()), &request) {
        AlwaysPrivatePlan::Refuse { message } => {
            assert!(!message.is_empty(), "a refusal carries fixed vocabulary");
        }
        AlwaysPrivatePlan::Update(next) => {
            assert!(
                !next.private_default,
                "absent toggle preserves stored off, never enables"
            );
        }
    }
}

#[test]
fn explicit_toggles_set_the_stored_value() {
    // Control: explicit input behaves exactly as today, in both directions.
    for enabled in [true, false] {
        let text = TextChannelsRequest {
            enabled: Some(enabled),
            name: None,
            viewer_role: None,
        };
        let TextChannelsPlan::Update(next) = decide_text_channels(Some(creator()), &text) else {
            panic!("explicit textchannels toggle applies, want Update({enabled})");
        };
        assert_eq!(next.text_channels, enabled);

        let group = GroupRequest {
            enabled: Some(enabled),
        };
        let GroupPlan::Update(next) = decide_group(Some(creator()), &group) else {
            panic!("explicit group toggle applies, want Update({enabled})");
        };
        assert_eq!(next.group_by_category, enabled);

        let private = AlwaysPrivateRequest {
            enabled: Some(enabled),
        };
        let AlwaysPrivatePlan::Update(next) = decide_always_private(Some(creator()), &private)
        else {
            panic!("explicit alwaysprivate toggle applies, want Update({enabled})");
        };
        assert_eq!(next.private_default, enabled);
    }
}

#[test]
fn unknown_creator_still_refuses() {
    // Control: the not-a-creator refusal is unchanged, with or without input.
    let TextChannelsPlan::Refuse { message } = decide_text_channels(
        None,
        &TextChannelsRequest {
            enabled: None,
            name: None,
            viewer_role: None,
        },
    ) else {
        panic!("unknown creator refuses textchannels");
    };
    assert!(message.contains("voice-room creator"), "{message}");

    let GroupPlan::Refuse { message } = decide_group(None, &GroupRequest { enabled: None }) else {
        panic!("unknown creator refuses group");
    };
    assert!(message.contains("voice-room creator"), "{message}");

    let AlwaysPrivatePlan::Refuse { message } =
        decide_always_private(None, &AlwaysPrivateRequest { enabled: None })
    else {
        panic!("unknown creator refuses alwaysprivate");
    };
    assert!(message.contains("voice-room creator"), "{message}");
}
