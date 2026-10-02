//! Hermetic V3a acceptance cases against the public core API.

use proptest::prelude::*;
use two_bot_core::voice_room_controls::{
    clamp_bitrate, name_conflicts, parse_limit, reset_bitrate_preference, room_bitrate,
    tier_max_bps, unlimit, validate_bitrate_preference, BitrateError, BitrateTier, LimitError,
    RoomLimit, MAX_ROOM_LIMIT, MIN_BITRATE_BPS, RESET_BITRATE_PREFERENCE,
};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn property_room_bitrate_always_stays_within_bounds(
        prefs in proptest::collection::vec(proptest::option::of(any::<u32>()), 0..8),
        creator_default in any::<u32>(),
        tier in proptest::sample::select(&[
            BitrateTier::Base,
            BitrateTier::Level1,
            BitrateTier::Level2,
            BitrateTier::Level3,
        ]),
    ) {
        let tier_max = tier_max_bps(tier);
        let bitrate = room_bitrate(&prefs, creator_default, tier_max);
        prop_assert!(bitrate >= MIN_BITRATE_BPS, "bitrate {bitrate} below minimum");
        prop_assert!(bitrate <= tier_max, "bitrate {bitrate} above tier max {tier_max}");
    }

    #[test]
    fn property_lock_never_produces_limit_above_99(headcount in any::<u32>()) {
        let limit = parse_limit(None, headcount).unwrap();
        prop_assert!(limit.user_limit() <= MAX_ROOM_LIMIT);
        if headcount == 0 {
            prop_assert_eq!(limit, RoomLimit::Unlimited);
        } else {
            prop_assert_eq!(limit.user_limit(), headcount.min(MAX_ROOM_LIMIT));
        }
    }
}

#[test]
fn limit_table_covers_every_edge() {
    // (arg, headcount, expected)
    let cases: &[(Option<u32>, u32, Result<RoomLimit, LimitError>)] = &[
        (Some(0), 0, Ok(RoomLimit::Unlimited)),
        (Some(0), 5, Ok(RoomLimit::Unlimited)),
        (Some(1), 5, Ok(RoomLimit::Limited(1))),
        (Some(99), 5, Ok(RoomLimit::Limited(99))),
        (Some(100), 5, Err(LimitError::OutOfRange(100))),
        (Some(101), 5, Err(LimitError::OutOfRange(101))),
        (Some(u32::MAX), 5, Err(LimitError::OutOfRange(u32::MAX))),
        (None, 0, Ok(RoomLimit::Unlimited)),
        (None, 1, Ok(RoomLimit::Limited(1))),
        (None, 42, Ok(RoomLimit::Limited(42))),
        (None, 99, Ok(RoomLimit::Limited(99))),
        // Lock clamps so it never produces a limit above 99.
        (None, 100, Ok(RoomLimit::Limited(99))),
        (None, 150, Ok(RoomLimit::Limited(99))),
        (None, u32::MAX, Ok(RoomLimit::Limited(99))),
    ];
    for (arg, headcount, expected) in cases {
        assert_eq!(&parse_limit(*arg, *headcount), expected, "arg={arg:?}");
    }
}

#[test]
fn unlimit_clears_the_limit() {
    assert_eq!(unlimit(), RoomLimit::Unlimited);
    assert_eq!(RoomLimit::unlimited(), RoomLimit::Unlimited);
    assert!(RoomLimit::Unlimited.is_unlimited());
    assert!(!RoomLimit::Limited(1).is_unlimited());
    assert_eq!(RoomLimit::Unlimited.user_limit(), 0);
    assert_eq!(RoomLimit::Limited(99).user_limit(), 99);
}

#[test]
fn tier_maxima_form_the_validation_table() {
    assert_eq!(tier_max_bps(BitrateTier::Base), 64_000);
    assert_eq!(tier_max_bps(BitrateTier::Level1), 128_000);
    assert_eq!(tier_max_bps(BitrateTier::Level2), 256_000);
    assert_eq!(tier_max_bps(BitrateTier::Level3), 384_000);
}

#[test]
fn bitrate_validation_table_covers_every_edge() {
    // Below-or-at 8 kbps is always too low, regardless of tier.
    for value in [0, 1, 7_999, 8_000] {
        assert_eq!(
            validate_bitrate_preference(value, 64_000),
            Err(BitrateError::TooLow(value)),
            "value={value}"
        );
    }
    // Just above 8 kbps is accepted.
    assert_eq!(validate_bitrate_preference(8_001, 64_000), Ok(8_001));
    // Every tier accepts its own maximum and rejects one above it.
    for tier in [
        BitrateTier::Base,
        BitrateTier::Level1,
        BitrateTier::Level2,
        BitrateTier::Level3,
    ] {
        let max = tier_max_bps(tier);
        assert_eq!(validate_bitrate_preference(max, max), Ok(max));
        assert_eq!(
            validate_bitrate_preference(max + 1, max),
            Err(BitrateError::ExceedsTierMax {
                value: max + 1,
                tier_max: max
            }),
            "tier={tier:?}"
        );
    }
    // A too-low value reports TooLow even when it also exceeds a degenerate max.
    assert_eq!(
        validate_bitrate_preference(8_000, 8_000),
        Err(BitrateError::TooLow(8_000))
    );
}

#[test]
fn reset_value_clears_the_preference() {
    assert_eq!(reset_bitrate_preference(), None);
    assert_eq!(RESET_BITRATE_PREFERENCE, None);
    let prefs: &[Option<u32>] = &[reset_bitrate_preference()];
    assert_eq!(room_bitrate(prefs, 64_000, 64_000), 64_000);
}

#[test]
fn room_average_falls_back_and_clamps() {
    // No preferences: creator default passes through when already valid.
    assert_eq!(room_bitrate(&[], 32_000, 64_000), 32_000);
    assert_eq!(room_bitrate(&[None, None], 32_000, 64_000), 32_000);
    // No preferences: fallback is still clamped into range.
    assert_eq!(room_bitrate(&[], 1_000, 64_000), MIN_BITRATE_BPS);
    assert_eq!(room_bitrate(&[], 96_000, 64_000), 64_000);
    // Single preference wins.
    assert_eq!(room_bitrate(&[Some(32_000)], 64_000, 64_000), 32_000);
    // Unset members are ignored, not averaged as zero.
    assert_eq!(
        room_bitrate(&[Some(32_000), None, Some(64_000)], 8_001, 64_000),
        48_000
    );
    // Stored out-of-range preferences are tolerated through the final clamp.
    assert_eq!(room_bitrate(&[Some(1)], 64_000, 64_000), MIN_BITRATE_BPS);
    assert_eq!(room_bitrate(&[Some(u32::MAX)], 64_000, 64_000), 64_000);
}

#[test]
fn room_average_rounds_down() {
    // 16003 / 2 = 8001.5 floors to 8001.
    assert_eq!(
        room_bitrate(&[Some(8_001), Some(8_002)], 64_000, 64_000),
        8_001
    );
    // 24006 / 3 = 8002 exactly.
    assert_eq!(
        room_bitrate(&[Some(8_001), Some(8_002), Some(8_003)], 64_000, 64_000),
        8_002
    );
    // 64001 / 2 = 32000.5 floors to 32000.
    assert_eq!(
        room_bitrate(&[Some(32_000), Some(32_001)], 64_000, 64_000),
        32_000
    );
}

#[test]
fn clamp_keeps_every_value_in_range() {
    assert_eq!(clamp_bitrate(1, 64_000), MIN_BITRATE_BPS);
    assert_eq!(clamp_bitrate(8_000, 64_000), MIN_BITRATE_BPS);
    assert_eq!(clamp_bitrate(8_001, 64_000), 8_001);
    assert_eq!(clamp_bitrate(64_000, 64_000), 64_000);
    assert_eq!(clamp_bitrate(64_001, 64_000), 64_000);
    assert_eq!(clamp_bitrate(u32::MAX, 64_000), 64_000);
}

#[test]
fn name_conflict_is_case_sensitive_literal_match() {
    let existing = ["Lounge", "lounge room"];
    // Exact literal match conflicts when the setting is on.
    assert!(name_conflicts("Lounge", &existing, true));
    // Case-sensitive: different case is a different name.
    assert!(!name_conflicts("lounge", &existing, true));
    assert!(!name_conflicts("LOUNGE", &existing, true));
    // Literal only: no trimming or normalization.
    assert!(!name_conflicts(" Lounge", &existing, true));
    assert!(!name_conflicts("Lounge ", &existing, true));
    // Absent name does not conflict.
    assert!(!name_conflicts("Den", &existing, true));
    assert!(!name_conflicts("Den", &[] as &[&str], true));
    // Setting off: even an exact match does not conflict.
    assert!(!name_conflicts("Lounge", &existing, false));
    assert!(!name_conflicts("Lounge", &[] as &[&str], false));
}
