//! Hermetic V12a acceptance cases against the public core API.
//!
//! Table tests pin the documented ledger rules (0/199/200/201 builds, Dec→Jan
//! reset, leap-day February); property tests prove monotonic denial within a
//! month key and the month-boundary invariant; shape tests pin the request
//! allowlist. No tests in this fixture use a database, Redis, Discord, or a
//! staging identity.

use proptest::prelude::*;
use two_bot_core::voice_assistant_cap::{
    validate_request_shape, AssistantRequest, AssistantTemplate, CapDecision, CapError, CapLedger,
    MonthKey, RequestShapeError, MONTHLY_BUILD_LIMIT,
};

const GUILD: u64 = 7;
const OTHER_GUILD: u64 = 9;

// Pinned UTC timestamps (`date -u -d "<utc>" +%s`).
const DEC_2025_START: i64 = 1_764_547_200; // 2025-12-01 00:00:00 UTC
const DEC_31_2025_END: i64 = 1_767_225_599; // 2025-12-31 23:59:59 UTC
const JAN_2026_START: i64 = 1_767_225_600; // 2026-01-01 00:00:00 UTC
const JAN_15_2026_NOON: i64 = 1_768_478_400; // 2026-01-15 12:00:00 UTC
const FEB_2026_START: i64 = 1_769_904_000; // 2026-02-01 00:00:00 UTC
const MAR_2026_START: i64 = 1_772_323_200; // 2026-03-01 00:00:00 UTC
const FEB_2024_START: i64 = 1_706_745_600; // 2024-02-01 00:00:00 UTC (leap year)
const FEB_29_2024_NOON: i64 = 1_709_208_000; // 2024-02-29 12:00:00 UTC
const MAR_2024_START: i64 = 1_709_251_200; // 2024-03-01 00:00:00 UTC

fn jan(year: i32) -> MonthKey {
    MonthKey { year, month: 1 }
}

fn dec_2025() -> MonthKey {
    MonthKey {
        year: 2025,
        month: 12,
    }
}

#[test]
fn builds_below_limit_allow_with_remaining() {
    let mut ledger = CapLedger::new();
    // First build of the month: 199 left after it.
    assert_eq!(
        ledger.record(GUILD, JAN_2026_START, 0, JAN_15_2026_NOON),
        Ok(CapDecision::Allow { remaining: 199 })
    );
    assert_eq!(ledger.usage(GUILD), Some((jan(2026), 1)));
    // 199th build used: this is the last allowed one, none left after.
    assert_eq!(
        ledger.record(GUILD, JAN_2026_START, 199, JAN_15_2026_NOON),
        Ok(CapDecision::Allow { remaining: 0 })
    );
    assert_eq!(ledger.usage(GUILD), Some((jan(2026), 200)));
}

#[test]
fn builds_at_and_over_limit_deny_with_reset() {
    let mut ledger = CapLedger::new();
    assert_eq!(MONTHLY_BUILD_LIMIT, 200);
    assert_eq!(
        ledger.record(GUILD, JAN_2026_START, 200, JAN_15_2026_NOON),
        Ok(CapDecision::Deny {
            limit: 200,
            resets_at: FEB_2026_START,
        })
    );
    // A denial counts nothing: the mirror keeps the stored row.
    assert_eq!(ledger.usage(GUILD), Some((jan(2026), 200)));
    // Over-cap rows (stale or racing writers) deny identically.
    assert_eq!(
        ledger.record(GUILD, JAN_2026_START, 201, JAN_15_2026_NOON),
        Ok(CapDecision::Deny {
            limit: 200,
            resets_at: FEB_2026_START,
        })
    );
    assert_eq!(ledger.usage(GUILD), Some((jan(2026), 201)));
}

#[test]
fn december_cap_resets_on_january_first() {
    let mut ledger = CapLedger::new();
    // Capped on the last second of December: reset is Jan 1st 00:00 UTC.
    assert_eq!(
        ledger.record(GUILD, DEC_2025_START, 200, DEC_31_2025_END),
        Ok(CapDecision::Deny {
            limit: 200,
            resets_at: JAN_2026_START,
        })
    );
    // The same stored row under a January clock resets to zero: allowed.
    assert_eq!(
        ledger.record(GUILD, DEC_2025_START, 200, JAN_2026_START),
        Ok(CapDecision::Allow { remaining: 199 })
    );
    assert_eq!(ledger.usage(GUILD), Some((jan(2026), 1)));
    // One build short of the cap on New Year's Eve is still allowed.
    assert_eq!(
        ledger.record(GUILD, DEC_2025_START, 199, DEC_31_2025_END),
        Ok(CapDecision::Allow { remaining: 0 })
    );
}

#[test]
fn leap_day_february_caps_and_march_resets() {
    // Feb 29th belongs to February: no special case in the math.
    assert_eq!(
        MonthKey::from_unix_secs(FEB_29_2024_NOON),
        Ok(MonthKey {
            year: 2024,
            month: 2
        })
    );
    let mut ledger = CapLedger::new();
    assert_eq!(
        ledger.record(GUILD, FEB_2024_START, 199, FEB_29_2024_NOON),
        Ok(CapDecision::Allow { remaining: 0 })
    );
    assert_eq!(
        ledger.record(GUILD, FEB_2024_START, 200, FEB_29_2024_NOON),
        Ok(CapDecision::Deny {
            limit: 200,
            resets_at: MAR_2024_START,
        })
    );
    // March 1st resets even a capped leap February.
    assert_eq!(
        ledger.record(GUILD, FEB_2024_START, 200, MAR_2024_START),
        Ok(CapDecision::Allow { remaining: 199 })
    );
    // Non-leap February 2026: the second before March is still February.
    assert_eq!(
        MonthKey::from_unix_secs(MAR_2026_START - 1),
        Ok(MonthKey {
            year: 2026,
            month: 2
        })
    );
    assert_eq!(
        ledger.record(GUILD, FEB_2026_START, 200, MAR_2026_START - 1),
        Ok(CapDecision::Deny {
            limit: 200,
            resets_at: MAR_2026_START,
        })
    );
    assert_eq!(
        ledger.record(GUILD, FEB_2026_START, 200, MAR_2026_START),
        Ok(CapDecision::Allow { remaining: 199 })
    );
}

#[test]
fn month_keys_and_boundaries() {
    assert_eq!(MonthKey::from_unix_secs(DEC_31_2025_END), Ok(dec_2025()));
    assert_eq!(MonthKey::from_unix_secs(JAN_2026_START), Ok(jan(2026)));
    assert_eq!(dec_2025().start_unix_secs(), Ok(DEC_2025_START));
    assert_eq!(dec_2025().next_start_unix_secs(), Ok(JAN_2026_START));
    assert_eq!(jan(2026).start_unix_secs(), Ok(JAN_2026_START));
    assert_eq!(jan(2026).next_start_unix_secs(), Ok(FEB_2026_START));
    assert_eq!(
        MonthKey {
            year: 2024,
            month: 2
        }
        .next_start_unix_secs(),
        Ok(MAR_2024_START)
    );
    assert_eq!(
        MonthKey {
            year: 2026,
            month: 2
        }
        .next_start_unix_secs(),
        Ok(MAR_2026_START)
    );
}

#[test]
fn decisions_report_helpers() {
    let allow = CapDecision::Allow { remaining: 3 };
    assert!(allow.allowed());
    assert_eq!(allow.remaining(), Some(3));
    assert_eq!(allow.resets_at(), None);
    let deny = CapDecision::Deny {
        limit: 200,
        resets_at: FEB_2026_START,
    };
    assert!(!deny.allowed());
    assert_eq!(deny.remaining(), None);
    assert_eq!(deny.resets_at(), Some(FEB_2026_START));
}

#[test]
fn guilds_are_isolated() {
    let mut ledger = CapLedger::new();
    assert_eq!(
        ledger.record(GUILD, JAN_2026_START, 200, JAN_15_2026_NOON),
        Ok(CapDecision::Deny {
            limit: 200,
            resets_at: FEB_2026_START,
        })
    );
    // A capped guild changes nothing for its neighbour.
    assert_eq!(
        ledger.record(OTHER_GUILD, JAN_2026_START, 0, JAN_15_2026_NOON),
        Ok(CapDecision::Allow { remaining: 199 })
    );
    assert_eq!(ledger.usage(GUILD), Some((jan(2026), 200)));
    assert_eq!(ledger.usage(OTHER_GUILD), Some((jan(2026), 1)));
    assert_eq!(ledger.usage(12345), None);
}

#[test]
fn invalid_inputs_are_refused() {
    let mut ledger = CapLedger::new();
    assert_eq!(
        ledger.record(0, JAN_2026_START, 0, JAN_15_2026_NOON),
        Err(CapError::InvalidGuildId)
    );
    // A month start that is not the 1st at 00:00 UTC is a caller bug.
    assert_eq!(
        ledger.record(GUILD, JAN_15_2026_NOON, 0, JAN_15_2026_NOON),
        Err(CapError::InvalidMonthStart)
    );
    // The clock moving before the stored row refuses rather than re-granting.
    assert_eq!(
        ledger.record(GUILD, FEB_2026_START, 0, JAN_15_2026_NOON),
        Err(CapError::ClockRollback)
    );
    assert_eq!(
        MonthKey::from_unix_secs(i64::MAX),
        Err(CapError::TimestampOutOfRange)
    );
    // A hand-built key outside 1..=12 is caller misuse, refused instead of
    // silently wrapping (month 13 would otherwise read as January).
    for month in [0, 13, u32::MAX] {
        assert_eq!(
            MonthKey { year: 2026, month }.start_unix_secs(),
            Err(CapError::InvalidMonthStart)
        );
    }
    assert!(CapLedger::new().usage(GUILD).is_none());
}

// ---- Request shape: the §V12 allowlist ----

fn template(channel_id: u64, name: &str) -> AssistantTemplate<'_> {
    AssistantTemplate {
        channel_id,
        name_template: name,
        status_template: None,
    }
}

fn valid_request<'a>(
    prompt: &'a str,
    guild_templates: &'a [AssistantTemplate<'a>],
) -> AssistantRequest<'a> {
    AssistantRequest {
        prompt,
        guild_templates,
        no_game_label: "General",
        locale: "en",
    }
}

#[test]
fn valid_request_shapes_pass() {
    let templates = [
        template(11, "@@owner@@'s den ##"),
        AssistantTemplate {
            channel_id: 12,
            name_template: "@@game_name@@ ##",
            status_template: Some("@@num@@ playing"),
        },
    ];
    assert!(validate_request_shape(&valid_request("Cozy den names", &templates)).is_ok());
    let bare = [];
    for locale in ["en", "en-US", "pt-BR", "zh-Hans", "de-1996"] {
        let request = AssistantRequest {
            locale,
            ..valid_request("Bitte gemütliche Namen", &bare)
        };
        assert!(
            validate_request_shape(&request).is_ok(),
            "locale {locale} should pass"
        );
    }
}

#[test]
fn request_shape_rejects_bad_prompts_labels_and_locales() {
    let bare = [];
    let blank_prompt = valid_request("   ", &bare);
    assert_eq!(
        validate_request_shape(&blank_prompt),
        Err(RequestShapeError::EmptyPrompt)
    );
    let long_prompt = "x".repeat(2001);
    assert_eq!(
        validate_request_shape(&valid_request(&long_prompt, &bare)),
        Err(RequestShapeError::PromptTooLong)
    );
    let blank_label = AssistantRequest {
        no_game_label: "  ",
        ..valid_request("names", &bare)
    };
    assert_eq!(
        validate_request_shape(&blank_label),
        Err(RequestShapeError::EmptyNoGameLabel)
    );
    let long_label = "x".repeat(101);
    assert_eq!(
        validate_request_shape(&AssistantRequest {
            no_game_label: &long_label,
            ..valid_request("names", &bare)
        }),
        Err(RequestShapeError::NoGameLabelTooLong)
    );
    let blank_locale = AssistantRequest {
        locale: "",
        ..valid_request("names", &bare)
    };
    assert_eq!(
        validate_request_shape(&blank_locale),
        Err(RequestShapeError::EmptyLocale)
    );
    for locale in ["x", "1en", "-en", "en-", "en--US", "en_us", "en us"] {
        let request = AssistantRequest {
            locale,
            ..valid_request("names", &bare)
        };
        assert_eq!(
            validate_request_shape(&request),
            Err(RequestShapeError::InvalidLocale),
            "locale {locale:?} should fail"
        );
    }
    let too_long_locale = "a".repeat(36);
    assert_eq!(
        validate_request_shape(&AssistantRequest {
            locale: &too_long_locale,
            ..valid_request("names", &bare)
        }),
        Err(RequestShapeError::InvalidLocale)
    );
}

#[test]
fn request_shape_rejects_bad_templates() {
    let zero_id = [template(0, "@@owner@@ ##")];
    assert_eq!(
        validate_request_shape(&valid_request("names", &zero_id)),
        Err(RequestShapeError::InvalidChannelId)
    );
    let duplicate = [template(11, "a ##"), template(11, "b ##")];
    assert_eq!(
        validate_request_shape(&valid_request("names", &duplicate)),
        Err(RequestShapeError::DuplicateTemplateChannel)
    );
    let blank = [template(11, "   ")];
    assert_eq!(
        validate_request_shape(&valid_request("names", &blank)),
        Err(RequestShapeError::EmptyTemplate)
    );
    let long_name = "x".repeat(4097);
    let long = [AssistantTemplate {
        channel_id: 11,
        name_template: &long_name,
        status_template: None,
    }];
    assert_eq!(
        validate_request_shape(&valid_request("names", &long)),
        Err(RequestShapeError::TemplateTooLong)
    );
    let long_status = "x".repeat(4097);
    let long_st = [AssistantTemplate {
        channel_id: 11,
        name_template: "ok ##",
        status_template: Some(&long_status),
    }];
    assert_eq!(
        validate_request_shape(&valid_request("names", &long_st)),
        Err(RequestShapeError::StatusTemplateTooLong)
    );
    // 129 templates exceed the coarse payload bound.
    let many: Vec<AssistantTemplate<'_>> = (1..=129u64).map(|id| template(id, "ok ##")).collect();
    assert_eq!(
        validate_request_shape(&valid_request("names", &many)),
        Err(RequestShapeError::TooManyTemplates)
    );
}

// ---- Property tests ----

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_allow_remaining_is_exact(used in 0..MONTHLY_BUILD_LIMIT) {
        let mut ledger = CapLedger::new();
        let first = ledger.record(GUILD, JAN_2026_START, used, JAN_15_2026_NOON)
            .expect("in-month record");
        prop_assert_eq!(
            first,
            CapDecision::Allow {
                remaining: MONTHLY_BUILD_LIMIT - used - 1,
            }
        );
        // The same inputs decide the same way twice: the decision is a pure
        // function of the persisted row and the clock.
        let second = ledger.record(GUILD, JAN_2026_START, used, JAN_15_2026_NOON)
            .expect("in-month record");
        prop_assert_eq!(second, first);
    }

    #[test]
    fn prop_denied_stays_denied_within_month(used in MONTHLY_BUILD_LIMIT..=u32::MAX) {
        let mut ledger = CapLedger::new();
        let expected = CapDecision::Deny {
            limit: MONTHLY_BUILD_LIMIT,
            resets_at: FEB_2026_START,
        };
        prop_assert_eq!(
            ledger.record(GUILD, JAN_2026_START, used, JAN_15_2026_NOON)
                .expect("in-month record"),
            expected
        );
        // Monotonic: one more build used is still denied with the same reset.
        if let Some(more) = used.checked_add(1) {
            prop_assert_eq!(
                ledger.record(GUILD, JAN_2026_START, more, JAN_15_2026_NOON)
                    .expect("in-month record"),
                expected
            );
        }
        // A later clock in the same month key is still denied.
        let late_january = FEB_2026_START - 86_400;
        prop_assert_eq!(
            ledger.record(GUILD, JAN_2026_START, used, late_january)
                .expect("in-month record"),
            expected
        );
    }

    #[test]
    fn prop_month_key_contains_now(now in 0i64..2_000_000_000i64) {
        let key = MonthKey::from_unix_secs(now).expect("in-range timestamp");
        let start = key.start_unix_secs().expect("month start");
        let next = key.next_start_unix_secs().expect("next reset");
        // The reset boundary is exact: start <= now < next.
        prop_assert!(start <= now && now < next, "now {now} outside [{start}, {next})");
        // The start belongs to its own key; the second before the reset to
        // the old one; the reset itself to a strictly newer key.
        prop_assert_eq!(
            MonthKey::from_unix_secs(start).expect("month start"),
            key
        );
        prop_assert_eq!(
            MonthKey::from_unix_secs(next - 1).expect("pre-reset second"),
            key
        );
        prop_assert!(
            MonthKey::from_unix_secs(next).expect("reset instant") > key,
            "reset {next} must open a newer month than {key:?}"
        );
    }
}
