//! Protected acceptance: existing boot/hot readers and write validation agree.
//! Break caught: a renderer/parser drift stores a value that breaks refresh.
//! These are syntax tests, not evidence of channel existence or permissions.

#[path = "../../core/tests/support/onboarding_destination_cases.rs"]
mod cases;

use std::collections::HashMap;
use two_bot_core::settings::{to_env_string, validate_write};
use two_bot_discord::onboarding_config::{OnboardingConfig, OnboardingConfigError};

fn vars(key: &str, value: &str) -> HashMap<String, String> {
    let mut vars = HashMap::from([
        ("DISCORD_GUILD_ID".into(), "111111111111111111".into()),
        ("DISCORD_ANCHOR_WELCOME_CHANNEL_ID".into(), "51".into()),
        (
            "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID".into(),
            "52".into(),
        ),
        ("DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID".into(), "53".into()),
        (
            "TWO_ONBOARDING_MODE".into(),
            if key == cases::SINGLES[0] {
                "anchor"
            } else {
                "session"
            }
            .into(),
        ),
    ]);
    vars.insert(key.into(), value.into());
    vars
}

#[test]
fn reader_positive_control_matches_literal_rendering_and_parsed_ids() {
    for key in cases::ALL {
        for (value, rendered, expected) in cases::valid(key) {
            assert_eq!(to_env_string(&value).as_deref(), Some(rendered), "{key}");
            let parsed = OnboardingConfig::from_map(&vars(key, rendered));
            if cases::SINGLES.contains(&key) && expected.is_empty() {
                assert!(matches!(parsed, Err(OnboardingConfigError::Missing(k)) if k == key));
                continue;
            }
            let config = parsed.unwrap().unwrap();
            let actual = match key {
                "DISCORD_ANCHOR_WELCOME_CHANNEL_ID" => {
                    config.anchor_channel_id.into_iter().collect()
                }
                "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID" => {
                    vec![config.session_picks[0].channel_id.parse::<u64>().unwrap()]
                }
                "DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID" => {
                    vec![config.session_picks[1].channel_id.parse::<u64>().unwrap()]
                }
                "DISCORD_LANDING_CHANNEL_IDS" => config.landing_channel_ids,
                "DISCORD_GOODBYE_CHANNEL_IDS" => config.goodbye_channel_ids,
                _ => unreachable!("fixture key"),
            };
            assert_eq!(actual, expected, "{key}");
        }
    }
}

#[test]
fn reader_negative_control_rejects_every_renderable_invalid_fixture() {
    for key in cases::ALL {
        for value in cases::invalid(key) {
            if let Some(rendered) = to_env_string(&value) {
                assert!(matches!(OnboardingConfig::from_map(&vars(key, &rendered)),
                    Err(OnboardingConfigError::InvalidId(k)) if k == key));
            } else {
                assert!(value.is_null(), "only stored null is ignored by rendering");
            }
        }
    }
}

#[test]
fn malformed_reader_input_is_also_refused_at_write_time() {
    for key in cases::ALL {
        for value in cases::invalid(key) {
            assert!(
                validate_write("111111111111111111", key, Some(value), "222222222222222222")
                    .is_err(),
                "{key}: an unusable override must not be saved for a later mode"
            );
        }
    }
}

#[test]
fn optional_empty_delete_and_mode_requiredness_controls_are_preserved() {
    assert!(OnboardingConfig::from_map(&HashMap::new())
        .unwrap()
        .is_none());
    let mut base = HashMap::from([("DISCORD_GUILD_ID".into(), "111111111111111111".into())]);
    let config = OnboardingConfig::from_map(&base).unwrap().unwrap();
    assert!(config.anchor_channel_id.is_none());
    assert!(config.session_picks.is_empty());
    assert!(config.landing_channel_ids.is_empty());
    assert!(config.goodbye_channel_ids.is_empty());
    // Legacy does not require the session destinations, even when malformed.
    base.insert(cases::SINGLES[1].into(), "not-an-id".into());
    base.insert(cases::SINGLES[2].into(), "not-an-id".into());
    assert!(OnboardingConfig::from_map(&base).is_ok());
    base.insert("TWO_ONBOARDING_MODE".into(), "anchor".into());
    for empty in [None, Some(""), Some(" \t")] {
        if let Some(empty) = empty {
            base.insert(cases::SINGLES[0].into(), empty.into());
        } else {
            base.remove(cases::SINGLES[0]);
        }
        assert!(
            matches!(OnboardingConfig::from_map(&base), Err(OnboardingConfigError::Missing(k)) if k == cases::SINGLES[0])
        );
    }
    let mut session = vars(cases::SINGLES[1], "52");
    for key in &cases::SINGLES[1..] {
        let old = session.remove(*key).unwrap();
        assert!(
            matches!(OnboardingConfig::from_map(&session), Err(OnboardingConfigError::Missing(k)) if k == *key)
        );
        session.insert((*key).into(), old);
    }
    for dry in ["0", "1"] {
        session.insert("TWO_ONBOARDING_DRY_RUN".into(), dry.into());
        let config = OnboardingConfig::from_map(&session).unwrap().unwrap();
        assert_eq!(config.gates.dry_run, dry == "1");
        assert_eq!(config.session_picks.len(), 2);
    }
    session.insert("TWO_ONBOARDING_DRY_RUN".into(), "2".into());
    assert!(matches!(
        OnboardingConfig::from_map(&session),
        Err(OnboardingConfigError::Gates(_))
    ));
}
