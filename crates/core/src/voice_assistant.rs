//! Template-assistant wiring part 1: config gate and command shape (spec
//! `docs/voice-rooms.md` §V12).
//!
//! Original implementation written from the behaviour spec only. §V12 is
//! optional and config-gated: the assistant stays disabled unless an
//! OpenAI-compatible endpoint is configured. This module owns the gate read
//! and the `/templateassistant` slash-command shape; it performs no I/O,
//! holds no credentials and knows no Discord wire types.
//!
//! Gate rules, in order:
//!
//! 1. [`AssistantConfig::from_map`] returns `Some` only when
//!    `TWO_ASSISTANT_ENDPOINT` is present, trims to non-empty text, carries
//!    an `http://` or `https://` scheme, holds no control characters and fits
//!    [`MAX_ENDPOINT_CHARS`]. Anything else means disabled — the same
//!    fail-closed posture as [`crate::voice_rooms::VoiceGates`].
//! 2. `TWO_ASSISTANT_MODEL` trims into the config but never gates it: the
//!    spec disables on a missing endpoint only. A blank model is refused at
//!    call time by the V12b [`crate::voice_assistant_request::ModelName`]
//!    check, never by truncating to a different model.
//! 3. [`assistant_command_set`] publishes the command only when the voice
//!    gate (`TWO_VOICE=1`) and the assistant gate are both on. The command
//!    itself is admin-gated (Manage Guild) with one required `request`
//!    option bounded by the V12b [`crate::voice_assistant_request`] limit.
//!
//! Residual parent work (later slices): the per-guild monthly-cap DB column
//! (the V12a [`crate::voice_assistant_cap`] ledger consumes the persisted
//! row), the endpoint call with the validated V12b payload, the six-scenario
//! V12c validation before the admin sees output, the Apply/Refine/Cancel
//! flow and the credential binding for the endpoint. Publication already runs
//! through `InteractionRouter::publish_set` while both gates are on.

use std::collections::HashMap;

use super::commands::{CommandDefinition, CommandOption, CommandOptionType, PERM_MANAGE_GUILD};
use super::voice_assistant_request::MAX_REQUEST_CHARS;
use crate::voice_rooms::VoiceGates;

/// Longest assistant endpoint URL accepted from configuration, in characters.
/// Generous for hostnames plus a path prefix; longer values are refused and
/// the assistant stays disabled rather than truncating to another address.
pub const MAX_ENDPOINT_CHARS: usize = 2048;

/// Configured assistant endpoint and model. `None` (from [`AssistantConfig::from_map`]) means
/// disabled: no command is published and no endpoint call may happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantConfig {
    endpoint: String,
    model: String,
}

impl AssistantConfig {
    /// Read the assistant gate from the process environment.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Self::from_map(&std::env::vars().collect())
    }

    /// Read the assistant gate from an explicit map (tests, staged config).
    /// Returns `None` — disabled — unless `TWO_ASSISTANT_ENDPOINT` trims to a
    /// non-empty `http(s)` URL without control characters that fits
    /// [`MAX_ENDPOINT_CHARS`].
    #[must_use]
    pub fn from_map(vars: &HashMap<String, String>) -> Option<Self> {
        let endpoint = vars.get("TWO_ASSISTANT_ENDPOINT")?.trim().to_string();
        if !valid_endpoint(&endpoint) {
            return None;
        }
        let model = vars
            .get("TWO_ASSISTANT_MODEL")
            .map_or(String::new(), |m| m.trim().to_string());
        Some(Self { endpoint, model })
    }

    /// The configured endpoint URL, trimmed.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The configured model name, trimmed; empty when unset (refused at call
    /// time by the V12b model check).
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

/// Fail-closed endpoint check: non-empty, fits the bound, an `http(s)` URL
/// with an address after the scheme, and no control characters (a stray
/// newline must disable, not redirect).
fn valid_endpoint(endpoint: &str) -> bool {
    if endpoint.is_empty() || endpoint.chars().count() > MAX_ENDPOINT_CHARS {
        return false;
    }
    if endpoint.chars().any(char::is_control) {
        return false;
    }
    let rest = if let Some(rest) = endpoint
        .get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("http://"))
        .map(|_| &endpoint[7..])
    {
        rest
    } else if let Some(rest) = endpoint
        .get(..8)
        .filter(|prefix| prefix.eq_ignore_ascii_case("https://"))
        .map(|_| &endpoint[8..])
    {
        rest
    } else {
        return false;
    };
    !rest.trim().is_empty()
}

/// V12 `/templateassistant` definition: admin-only (Manage Guild), one
/// required `request` option carrying the admin's plain-language naming
/// description. The admin's locale arrives in the interaction itself, so no
/// locale option is needed; member data has no option to travel in.
#[must_use]
pub fn assistant_commands() -> Vec<CommandDefinition> {
    vec![CommandDefinition::new(
        "templateassistant",
        "Describe the voice-room naming you want; the bot drafts a name template",
    )
    .permissions(PERM_MANAGE_GUILD)
    .options(vec![CommandOption::new(
        "request",
        "Plain-language description of the naming you want, in any language",
        CommandOptionType::String,
    )
    .required()
    .max_length(MAX_REQUEST_CHARS as u32)])]
}

/// Assistant definitions for the guild command merge: the V1 voice gate and
/// the assistant gate must both be on, otherwise nothing is published.
#[must_use]
pub fn assistant_command_set(
    voice: &VoiceGates,
    assistant: Option<&AssistantConfig>,
) -> Vec<CommandDefinition> {
    if voice.enabled && assistant.is_some() {
        assistant_commands()
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn disabled_by_default() {
        assert_eq!(AssistantConfig::from_map(&HashMap::new()), None);
    }

    #[test]
    fn blank_endpoint_stays_disabled() {
        for endpoint in ["", "   ", "\t\n "] {
            assert_eq!(
                AssistantConfig::from_map(&vars(&[("TWO_ASSISTANT_ENDPOINT", endpoint)])),
                None,
                "endpoint {endpoint:?}"
            );
        }
    }

    #[test]
    fn non_http_endpoint_stays_disabled() {
        for endpoint in [
            "example.com/v1",
            "ftp://example.com/v1",
            "wss://example.com/v1",
            "gopher://example.com",
            "https://",
            "http://",
            "http://   ",
        ] {
            assert_eq!(
                AssistantConfig::from_map(&vars(&[("TWO_ASSISTANT_ENDPOINT", endpoint)])),
                None,
                "endpoint {endpoint:?}"
            );
        }
    }

    #[test]
    fn control_characters_and_overlong_endpoints_stay_disabled() {
        assert_eq!(
            AssistantConfig::from_map(&vars(&[(
                "TWO_ASSISTANT_ENDPOINT",
                "https://x.invalid\n/v1"
            )])),
            None
        );
        let long = format!("https://x.invalid/{}", "a".repeat(MAX_ENDPOINT_CHARS));
        assert_eq!(
            AssistantConfig::from_map(&vars(&[("TWO_ASSISTANT_ENDPOINT", &long)])),
            None
        );
    }

    #[test]
    fn http_and_https_endpoints_enable_with_trimmed_values() {
        let config = AssistantConfig::from_map(&vars(&[
            ("TWO_ASSISTANT_ENDPOINT", "  https://example.com/v1  "),
            ("TWO_ASSISTANT_MODEL", "  example-model  "),
        ]))
        .expect("https endpoint enables");
        assert_eq!(config.endpoint(), "https://example.com/v1");
        assert_eq!(config.model(), "example-model");

        let config = AssistantConfig::from_map(&vars(&[(
            "TWO_ASSISTANT_ENDPOINT",
            "http://internal:8080/openai",
        )]))
        .expect("http endpoint enables");
        assert_eq!(
            config.model(),
            "",
            "unset model stays blank for the call-time check"
        );
    }

    #[test]
    fn command_shape_is_admin_with_one_bounded_request_option() {
        let defs = assistant_commands();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "templateassistant");
        assert_eq!(
            defs[0].default_member_permissions,
            Some(PERM_MANAGE_GUILD.to_string())
        );
        assert_eq!(defs[0].options.len(), 1);
        assert_eq!(defs[0].options[0].name, "request");
        assert_eq!(defs[0].options[0].required, Some(true));
        assert_eq!(
            defs[0].options[0].max_length,
            Some(MAX_REQUEST_CHARS as u32)
        );
    }

    #[test]
    fn command_set_needs_both_gates() {
        let voice_on = VoiceGates { enabled: true };
        let voice_off = VoiceGates::from_map(&HashMap::new());
        let assistant = AssistantConfig::from_map(&vars(&[(
            "TWO_ASSISTANT_ENDPOINT",
            "https://example.com/v1",
        )]))
        .expect("endpoint enables");

        assert!(assistant_command_set(&voice_off, Some(&assistant)).is_empty());
        assert!(assistant_command_set(&voice_on, None).is_empty());
        let names: Vec<_> = assistant_command_set(&voice_on, Some(&assistant))
            .iter()
            .map(|definition| definition.name.clone())
            .collect();
        assert_eq!(names, ["templateassistant"]);
    }
}
