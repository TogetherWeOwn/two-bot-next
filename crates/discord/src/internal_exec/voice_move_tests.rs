use super::*;
use twilight_http::request::Method;

#[test]
fn prepares_only_the_selected_channel_patch() {
    let factory = Client::new("synthetic-token".to_owned());
    let request = prepare_voice_move(&factory, "123", "456", "789").expect("valid IDs");
    assert_eq!(request.method(), Method::Patch);
    assert_eq!(request.path(), "guilds/123/members/456");
    assert!(request.headers().is_none());
    let body: serde_json::Value =
        serde_json::from_slice(request.body().expect("body")).expect("JSON body");
    assert_eq!(body, serde_json::json!({"channel_id": "789"}));
}

#[test]
fn invalid_ids_are_redacted_in_every_position() {
    let factory = Client::new("synthetic-token".to_owned());
    for invalid in ["0", "18446744073709551616", "invalid-sentinel", ""] {
        for position in 0..3 {
            let mut ids = ["1", "2", "3"];
            ids[position] = invalid;
            let error = match prepare_voice_move(&factory, ids[0], ids[1], ids[2]) {
                Ok(_) => panic!("invalid ID accepted"),
                Err(error) => error,
            };
            assert_eq!(error.code, ErrorCode::Malformed);
            assert_eq!(error.message, "Invalid Discord ID");
            assert_eq!(error.log_reason, "bad_discord_id");
            if !invalid.is_empty() {
                assert!(!format!("{error:?}").contains(invalid));
            }
        }
    }
    for position in 0..3 {
        let mut ids = ["1", "2", "3"];
        ids[position] = "18446744073709551615";
        assert!(prepare_voice_move(&factory, ids[0], ids[1], ids[2]).is_ok());
    }
}

#[test]
fn headers_only_classify_definitive_refusals() {
    for (status, expected) in [
        (400, Some(VoiceMovePatch::Stale)),
        (403, Some(VoiceMovePatch::Denied)),
        (404, Some(VoiceMovePatch::Stale)),
        (200, None),
        (204, None),
        (301, None),
        (429, None),
        (500, None),
        (503, None),
    ] {
        assert_eq!(header_refusal(status), expected);
    }
}

#[test]
fn delay_policy_selects_the_longest_valid_value_or_indefinite() {
    for (header, body, expected) in [
        (Some(2.0), Some(3.0), Some(3000)),
        (Some(3.0), Some(2.0), Some(3000)),
        (Some(0.0001), None, Some(1)),
        (Some(0.0), None, Some(0)),
        (Some(-1.0), Some(1.0), Some(1000)),
        (Some(f64::NAN), Some(0.25), Some(250)),
        (Some(f64::INFINITY), Some(0.5), Some(500)),
        (Some(f64::MAX), None, None),
        (None, None, None),
        (Some(-1.0), Some(f64::NEG_INFINITY), None),
        (Some(f64::NAN), None, None),
    ] {
        assert_eq!(retry_delay_ms(header, body), expected);
    }
}

#[test]
fn no_send_causes_remain_distinct() {
    let blocked = VoiceMovePatch::NotSent(VoiceMoveNotSent::Blocked);
    let storage = VoiceMovePatch::NotSent(VoiceMoveNotSent::Storage);
    let guard = VoiceMovePatch::NotSent(VoiceMoveNotSent::Guard);
    assert_ne!(blocked, storage);
    assert_ne!(blocked, guard);
    assert_ne!(storage, guard);
    assert_ne!(VoiceMovePatch::Unavailable, blocked);
    assert_ne!(VoiceMovePatch::Unavailable, storage);
    assert_ne!(VoiceMovePatch::Unavailable, guard);
}
