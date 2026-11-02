//! Acceptance tests: template-assistant disabled-unless-configured gate and
//! the named monthly-cap refusal at the before-send seam (spec
//! `docs/voice-rooms.md` §V12: "200 builds per guild per month, resetting on
//! the 1st" and "Disabled unless an OpenAI-compatible endpoint is
//! configured").
//!
//! Protected test file: the build card implements against these tests and
//! must not edit them. If a pinned name below cannot be implemented as
//! written, stop and escalate instead of changing the tests.
//!
//! Contract pinned here (build card implements exactly this, in
//! `two_bot_core::voice_assistant`, reusing the existing
//! [`AssistantConfig`] gate read and the V12a
//! `two_bot_core::voice_assistant_cap::CapDecision`):
//!
//! - `pub enum AssistantGateRefusal { NotConfigured, MonthlyCapReached {
//!   limit: u32, resets_at: i64 } }`, deriving at least
//!   `Debug, Clone, Copy, PartialEq, Eq`, implementing `Display` and
//!   `std::error::Error`.
//! - `AssistantGateRefusal::name(&self) -> &'static str` returns the stable
//!   machine-readable refusal names `"assistant_not_configured"` and
//!   `"monthly_cap_reached"`. The `Display` text contains the name; the
//!   cap refusal's `Display` text also contains the numeric limit and the
//!   numeric `resets_at` timestamp. These names are the "named refusal"
//!   text the cap check surfaces before any send.
//! - `pub fn require_send_allowed<'a>(config: Option<&'a AssistantConfig>,
//!   cap: CapDecision) -> Result<&'a AssistantConfig, AssistantGateRefusal>`
//!   is the single before-send gate: an unconfigured assistant is refused
//!   first, on every path, whatever the cap says; a configured assistant
//!   at the cap is refused with the cap's limit and reset instant; only a
//!   configured assistant under the cap passes, returning the config.
//!
//! No transport, credential, handler or endpoint-call code is introduced
//! by this contract: the gate only decides, it never sends. All endpoints
//! below are synthetic (`.invalid`) and are never contacted.

use std::collections::HashMap;
use std::error::Error;

use two_bot_core::voice_assistant::{
    assistant_command_set, require_send_allowed, AssistantConfig, AssistantGateRefusal,
};
use two_bot_core::voice_assistant_cap::{CapDecision, MONTHLY_BUILD_LIMIT};
use two_bot_core::voice_rooms::VoiceGates;

const SYNTHETIC_ENDPOINT: &str = "https://assistant.invalid/v1";
const FEB_2026_START: i64 = 1_769_904_000; // 2026-02-01 00:00:00 UTC

fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn configured() -> AssistantConfig {
    AssistantConfig::from_map(&vars(&[
        ("TWO_ASSISTANT_ENDPOINT", SYNTHETIC_ENDPOINT),
        ("TWO_ASSISTANT_MODEL", "synthetic-model"),
        ("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"),
    ]))
    .expect("synthetic endpoint must configure the assistant")
}

fn allow() -> CapDecision {
    CapDecision::Allow { remaining: 42 }
}

fn deny() -> CapDecision {
    CapDecision::Deny {
        limit: MONTHLY_BUILD_LIMIT,
        resets_at: FEB_2026_START,
    }
}

#[test]
fn unconfigured_refuses_even_when_cap_would_allow() {
    assert_eq!(
        require_send_allowed(None, allow()),
        Err(AssistantGateRefusal::NotConfigured)
    );
}

#[test]
fn unconfigured_refusal_wins_over_cap_denial() {
    // Disabled-by-default is checked first on every path: with no endpoint
    // configured, the refusal names the missing configuration, never the
    // cap, so no path can send.
    assert_eq!(
        require_send_allowed(None, deny()),
        Err(AssistantGateRefusal::NotConfigured)
    );
}

#[test]
fn blank_and_malformed_endpoints_are_all_unconfigured_and_refused() {
    let overlong = format!("https://{}", "a".repeat(2048));
    let cases: Vec<HashMap<String, String>> = vec![
        HashMap::new(),
        vars(&[("TWO_ASSISTANT_ENDPOINT", "")]),
        vars(&[("TWO_ASSISTANT_ENDPOINT", "   \t ")]),
        vars(&[("TWO_ASSISTANT_ENDPOINT", "ftp://assistant.invalid")]),
        vars(&[("TWO_ASSISTANT_ENDPOINT", "https://")]),
        vars(&[("TWO_ASSISTANT_ENDPOINT", "assistant.invalid/v1")]),
        vars(&[("TWO_ASSISTANT_ENDPOINT", "https://a.invalid\n.evil")]),
        vars(&[("TWO_ASSISTANT_ENDPOINT", overlong.as_str())]),
        // A model name alone never enables the assistant.
        vars(&[("TWO_ASSISTANT_MODEL", "synthetic-model")]),
    ];
    for case in &cases {
        let config = AssistantConfig::from_map(case);
        assert_eq!(config, None, "case must stay disabled: {case:?}");
        assert_eq!(
            require_send_allowed(config.as_ref(), allow()),
            Err(AssistantGateRefusal::NotConfigured),
            "unconfigured case must not send: {case:?}"
        );
    }
}

#[test]
fn configured_and_under_cap_passes_with_the_config() {
    let config = configured();
    let passed = require_send_allowed(Some(&config), allow())
        .expect("configured and under cap must pass the gate");
    assert_eq!(passed.endpoint(), SYNTHETIC_ENDPOINT);
    assert_eq!(passed, &config);
}

#[test]
fn configured_at_cap_refuses_with_named_text() {
    let config = configured();
    let refusal = require_send_allowed(Some(&config), deny())
        .expect_err("a guild at the monthly cap must be refused before send");
    assert_eq!(
        refusal,
        AssistantGateRefusal::MonthlyCapReached {
            limit: MONTHLY_BUILD_LIMIT,
            resets_at: FEB_2026_START,
        }
    );
    assert_eq!(refusal.name(), "monthly_cap_reached");
    let text = refusal.to_string();
    assert!(
        text.contains("monthly_cap_reached"),
        "display must carry the refusal name: {text}"
    );
    assert!(
        text.contains(&MONTHLY_BUILD_LIMIT.to_string()),
        "display must carry the limit: {text}"
    );
    assert!(
        text.contains(&FEB_2026_START.to_string()),
        "display must carry the reset instant: {text}"
    );
}

#[test]
fn refusal_names_are_stable_and_distinct() {
    let not_configured = AssistantGateRefusal::NotConfigured;
    let capped = AssistantGateRefusal::MonthlyCapReached {
        limit: MONTHLY_BUILD_LIMIT,
        resets_at: FEB_2026_START,
    };
    assert_eq!(not_configured.name(), "assistant_not_configured");
    assert_eq!(capped.name(), "monthly_cap_reached");
    assert_ne!(not_configured.name(), capped.name());
    assert!(not_configured.to_string().contains(not_configured.name()));
}

#[test]
fn gate_refusal_is_a_standard_error() {
    let refusal: Box<dyn Error> = Box::new(AssistantGateRefusal::NotConfigured);
    assert!(refusal.to_string().contains("assistant_not_configured"));
}

#[test]
fn unconfigured_publishes_no_command_on_any_path() {
    let voice_on = VoiceGates::from_map(&vars(&[("TWO_VOICE", "1")]));
    assert!(voice_on.enabled);
    // Voice on but assistant unconfigured: nothing is published, so the
    // command path cannot even be reached.
    assert!(assistant_command_set(&voice_on, None).is_empty());
    // Control: both gates on publishes exactly the one assistant command.
    let config = configured();
    let published = assistant_command_set(&voice_on, Some(&config));
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].name, "templateassistant");
}
