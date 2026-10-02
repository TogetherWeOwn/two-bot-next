//! Hermetic V10 logging-channel resolution acceptance cases.

use two_bot_core::voice_logging::{
    parse_detail_level, resolve_log_target, should_log, DetailLevel, LogTarget, LoggingCandidates,
    LoggingError, RepeatLedger, MAX_LEVEL_ECHO_CHARS, MAX_LOG_SENDS,
};

fn candidates(
    system: Option<u64>,
    dm: Option<u64>,
    dm_reachable: bool,
    creator: Option<u64>,
    setup: Option<u64>,
) -> LoggingCandidates {
    LoggingCandidates {
        system_channel_id: system,
        dm_user_id: dm,
        dm_reachable,
        creator_channel_id: creator,
        setup_user_id: setup,
    }
}

#[test]
fn targets_fall_back_in_order() {
    // Everything works: system channel wins, carrying the setup user mention.
    assert_eq!(
        resolve_log_target(candidates(Some(11), Some(22), true, Some(33), Some(44))),
        Some(LogTarget::SystemChannel {
            channel_id: 11,
            mention_user_id: Some(44),
        })
    );
    // No system channel: DM wins.
    assert_eq!(
        resolve_log_target(candidates(None, Some(22), true, Some(33), Some(44))),
        Some(LogTarget::DirectMessage { user_id: 22 })
    );
    // DM unreachable: creator chat wins.
    assert_eq!(
        resolve_log_target(candidates(None, Some(22), false, Some(33), Some(44))),
        Some(LogTarget::CreatorChat { channel_id: 33 })
    );
    // Nothing works: None.
    assert_eq!(
        resolve_log_target(candidates(None, Some(22), false, None, Some(44))),
        None
    );
    assert_eq!(
        resolve_log_target(candidates(None, None, true, None, None)),
        None
    );
}

#[test]
fn system_channel_posts_without_mention_when_no_setup_user() {
    assert_eq!(
        resolve_log_target(candidates(Some(11), Some(22), true, Some(33), None)),
        Some(LogTarget::SystemChannel {
            channel_id: 11,
            mention_user_id: None,
        })
    );
}

#[test]
fn zero_ids_count_as_absent() {
    assert_eq!(
        resolve_log_target(candidates(Some(0), Some(22), true, Some(33), Some(0))),
        Some(LogTarget::DirectMessage { user_id: 22 })
    );
    assert_eq!(
        resolve_log_target(candidates(Some(0), Some(0), true, Some(33), None)),
        Some(LogTarget::CreatorChat { channel_id: 33 })
    );
    assert_eq!(
        resolve_log_target(candidates(Some(0), Some(0), true, Some(0), None)),
        None
    );
}

#[test]
fn unknown_level_refused_fail_closed() {
    assert_eq!(parse_detail_level("off"), Ok(DetailLevel::Off));
    assert_eq!(parse_detail_level(" Brief "), Ok(DetailLevel::Brief));
    assert_eq!(parse_detail_level("FULL"), Ok(DetailLevel::Full));
    for raw in ["", "verbose", "everything", "offf"] {
        let Err(LoggingError::UnknownLevel { value }) = parse_detail_level(raw) else {
            panic!("{raw:?} must be refused");
        };
        assert!(value.chars().count() <= MAX_LEVEL_ECHO_CHARS);
    }
    // Long junk is truncated, never echoed in full.
    let Err(LoggingError::UnknownLevel { value }) = parse_detail_level(&"x".repeat(200)) else {
        panic!("long level must be refused");
    };
    assert_eq!(value.chars().count(), MAX_LEVEL_ECHO_CHARS);
}

#[test]
fn detail_gates_events() {
    assert!(!should_log(DetailLevel::Off, false));
    assert!(!should_log(DetailLevel::Off, true));
    assert!(should_log(DetailLevel::Brief, false));
    assert!(!should_log(DetailLevel::Brief, true));
    assert!(should_log(DetailLevel::Full, false));
    assert!(should_log(DetailLevel::Full, true));
    assert!(!DetailLevel::Off.is_enabled());
    assert!(DetailLevel::Brief.is_enabled());
    assert!(DetailLevel::Full.is_enabled());
}

#[test]
fn repeats_stop_after_the_bound() {
    let mut ledger = RepeatLedger::new();
    assert_eq!(ledger.sends(), 0);
    assert!(ledger.should_send());
    for expected in 1..=MAX_LOG_SENDS {
        assert!(ledger.record_send());
        assert_eq!(ledger.sends(), expected);
        assert_eq!(ledger.should_send(), expected < MAX_LOG_SENDS);
    }
    // Bound reached: further sends refused, count unchanged.
    assert!(!ledger.should_send());
    assert!(!ledger.record_send());
    assert_eq!(ledger.sends(), MAX_LOG_SENDS);
    assert!(!ledger.record_send());
    assert_eq!(ledger.sends(), MAX_LOG_SENDS);
    // A resolved-then-recurred failure restarts the budget.
    ledger.reset();
    assert_eq!(ledger.sends(), 0);
    assert!(ledger.should_send());
    assert!(ledger.record_send());
}
