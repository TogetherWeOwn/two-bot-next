//! Template-assistant wiring part 1: config gate and command shape (spec
//! `docs/voice-rooms.md` §V12).
//!
//! Original implementation written from the behaviour spec only. §V12 is
//! optional and config-gated: the assistant stays disabled unless an
//! OpenAI-compatible endpoint is configured. This module owns the gate read
//! and the `/templateassistant` slash-command shape; it performs no I/O,
//! holds its credential in [`crate::Secret`] and knows no Discord wire types.
//!
//! Gate rules, in order:
//!
//! 1. [`AssistantConfig::from_map`] returns `Some` only when
//!    `TWO_ASSISTANT_ENDPOINT` is present, trims to non-empty text, carries
//!    an `http://` or `https://` scheme, holds no control characters and fits
//!    [`MAX_ENDPOINT_CHARS`]. Userinfo, queries and fragments are
//!    refused. `TWO_ASSISTANT_API_KEY` must also be non-blank and control-free;
//!    no alias is accepted. Anything else means disabled — the same
//!    fail-closed posture as [`crate::voice_rooms::VoiceGates`].
//! 2. `TWO_ASSISTANT_MODEL` trims into the config but never gates it.
//!    A blank model is refused at
//!    call time by the V12b [`crate::voice_assistant_request::ModelName`]
//!    check, never by truncating to a different model.
//! 3. [`assistant_command_set`] publishes the command only when the voice
//!    gate (`TWO_VOICE=1`) and the assistant gate are both on. The command
//!    itself is admin-gated (Manage Guild) with one required `request`
//!    option bounded by the V12b [`crate::voice_assistant_request`] limit.
//!
//! Residual parent work (later slices): the endpoint call with the validated
//! V12b payload, the six-scenario V12c validation before the admin sees
//! output and the Apply/Refine/Cancel flow. The credential stays opaque until
//! a future authorization boundary. The per-guild monthly-cap row lives in `two-bot-store`
//! (`assistant_monthly_builds`, decided through the V12a
//! [`crate::voice_assistant_cap`] ledger) and the before-send gate below
//! refuses unconfigured assistants and capped guilds before any send.
//! Publication already runs through `InteractionRouter::publish_set` while
//! both gates are on.

use std::collections::HashMap;

use super::commands::{CommandDefinition, CommandOption, CommandOptionType, PERM_MANAGE_GUILD};
use super::voice_assistant_cap::CapDecision;
use super::voice_assistant_request::MAX_REQUEST_CHARS;
use crate::voice_rooms::VoiceGates;
use crate::Secret;

/// Longest assistant endpoint URL accepted from configuration, in characters.
/// Generous for hostnames plus a path prefix; longer values are refused and
/// the assistant stays disabled rather than truncating to another address.
pub const MAX_ENDPOINT_CHARS: usize = 2048;

/// Configured assistant endpoint, model and bound credential. `None` (from
/// [`AssistantConfig::from_map`]) means disabled: no command is published and
/// no endpoint call may happen.
///
/// Endpoint and credential use [`Secret`]; even a URL path may contain a
/// credential. `Debug` also redacts the supplied model string.
#[derive(Clone, PartialEq, Eq)]
pub struct AssistantConfig {
    endpoint: Secret<String>,
    model: String,
    api_key: Secret<String>,
}

impl std::fmt::Debug for AssistantConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssistantConfig")
            .field("endpoint", &self.endpoint)
            .field("model", &"[REDACTED]")
            .field("api_key", &self.api_key)
            .finish()
    }
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
    /// [`MAX_ENDPOINT_CHARS`], has no userinfo, query or fragment, and
    /// `TWO_ASSISTANT_API_KEY` is non-blank and control-free. Check controls
    /// before trimming the credential: a trailing CR/LF is not a valid binding.
    #[must_use]
    pub fn from_map(vars: &HashMap<String, String>) -> Option<Self> {
        let api_key = vars.get("TWO_ASSISTANT_API_KEY")?;
        if api_key.trim().is_empty() || api_key.chars().any(char::is_control) {
            return None;
        }
        let endpoint = vars.get("TWO_ASSISTANT_ENDPOINT")?.trim();
        if !valid_endpoint(endpoint) {
            return None;
        }
        let model = vars
            .get("TWO_ASSISTANT_MODEL")
            .map_or(String::new(), |m| m.trim().to_string());
        Some(Self {
            endpoint: Secret::new(endpoint.to_string()),
            model,
            api_key: Secret::new(api_key.trim().to_string()),
        })
    }

    /// The configured endpoint URL, trimmed; only for the transport boundary,
    /// never diagnostics. Userinfo, queries and fragments are refused.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        self.endpoint.expose()
    }

    /// The opaque credential for a future authorization-header boundary.
    /// The build pipeline must not expose it or place it in a URL or body.
    #[must_use]
    pub fn api_key(&self) -> &Secret<String> {
        &self.api_key
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
    // Parsing recovers an empty authority and erases an empty username
    // (`https://@host`); check the raw authority before normalization too.
    if rest
        .split(['/', '\\', '?', '#'])
        .next()
        .is_none_or(|authority| authority.trim().is_empty() || authority.contains('@'))
    {
        return false;
    }
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    // Inspect parsed userinfo too: HTTP URL parsing normalizes backslashes
    // and extra slashes. Do not let alternate spellings hide credentials.
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    // Neither query nor fragment is a credential channel. Refuse both
    // wholesale instead of guessing provider aliases/decoding rules.
    url.query().is_none() && url.fragment().is_none()
}

/// Why a build never reached the endpoint: the single before-send verdict.
/// The names are stable machine-readable text surfaced before any send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AssistantGateRefusal {
    /// No OpenAI-compatible endpoint is configured: the assistant stays
    /// fully disabled on every path.
    #[error("assistant_not_configured")]
    NotConfigured,
    /// The assistant is configured but the guild already used `limit`
    /// builds this month; the next attempt is allowed again at `resets_at`
    /// (the next 1st 00:00 UTC, as Unix seconds).
    #[error("monthly_cap_reached (limit {limit}, resets_at {resets_at})")]
    MonthlyCapReached { limit: u32, resets_at: i64 },
}

impl AssistantGateRefusal {
    /// The stable machine-readable refusal name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::NotConfigured => "assistant_not_configured",
            Self::MonthlyCapReached { .. } => "monthly_cap_reached",
        }
    }
}

/// The single before-send gate: decide, never send. An unconfigured
/// assistant is refused first, on every path, whatever the cap says; a
/// configured assistant at the monthly cap is refused with the cap's limit
/// and reset instant; only a configured assistant under the cap passes,
/// returning the config for the caller to send with.
pub fn require_send_allowed(
    config: Option<&AssistantConfig>,
    cap: CapDecision,
) -> Result<&AssistantConfig, AssistantGateRefusal> {
    let config = config.ok_or(AssistantGateRefusal::NotConfigured)?;
    match cap {
        CapDecision::Allow { .. } => Ok(config),
        CapDecision::Deny { limit, resets_at } => {
            Err(AssistantGateRefusal::MonthlyCapReached { limit, resets_at })
        }
    }
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
        std::iter::once(("TWO_ASSISTANT_API_KEY", "fixture-assistant-credential"))
            .chain(pairs.iter().copied())
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
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
    fn credential_is_retained_only_as_a_redacted_secret() {
        let config = AssistantConfig::from_map(&vars(&[
            ("TWO_ASSISTANT_ENDPOINT", "https://example.invalid/v1"),
            ("TWO_ASSISTANT_API_KEY", "  fixture-bound-credential  "),
        ]))
        .expect("bound credential enables");
        // Synthetic authorization-boundary witness: the gate retained the
        // binding, rather than merely checking for the key and discarding it.
        assert_eq!(config.api_key().expose(), "fixture-bound-credential");
        assert_eq!(format!("{}", config.api_key()), "[REDACTED]");
        assert_eq!(format!("{:?}", config.api_key()), "[REDACTED]");
        for credential in [
            "fixture-key\n",
            "\rfixture-key",
            "fixture-key\t",
            "\u{85}fixture-key",
        ] {
            assert!(AssistantConfig::from_map(&vars(&[
                ("TWO_ASSISTANT_ENDPOINT", "https://example.invalid/v1"),
                ("TWO_ASSISTANT_API_KEY", credential),
            ]))
            .is_none());
        }
    }

    #[test]
    fn encoded_credential_queries_and_normalized_userinfo_are_refused() {
        for endpoint in [
            "https://example.invalid/v1?%61pi%5fkey=fixture-value",
            "https://example.invalid/v1?ACCESS-TOKEN=fixture-value",
            "https://example.invalid/v1?client_secret=fixture-value",
            "https://example.invalid/v1?bearer_token=fixture-value",
            "https://example.invalid/v1?secret_key=fixture-value",
            "https://example.invalid/v1?api_secret=fixture-value",
            "https://example.invalid/v1?authorization=fixture-value",
            "https://example.invalid/v1?%2561pi_key=fixture-value",
            "https://example.invalid/v1?api+key=fixture-value",
            "https://example.invalid/v1?key%00=fixture-value",
            "https://example.invalid/v1?model=x&version=1",
            "https://example.invalid/v1?",
            "https://example.invalid/v1#docs",
            "https://example.invalid/v1#?key=fixture-value",
            "https://example.invalid/v1#",
            "https:///v1",
            "https://@example.invalid/v1",
            "https:////@example.invalid/v1",
            "https:////fixture-user:fixture-pass@example.invalid/v1",
            "https://\\\\fixture-user:fixture-pass@example.invalid/v1",
            "https://[invalid/v1",
        ] {
            assert!(
                AssistantConfig::from_map(&vars(&[("TWO_ASSISTANT_ENDPOINT", endpoint)])).is_none()
            );
        }
        let endpoint = "https://example.invalid/v1";
        let config = AssistantConfig::from_map(&vars(&[("TWO_ASSISTANT_ENDPOINT", endpoint)]))
            .expect("a clean endpoint preserves the binding");
        assert_eq!(config.endpoint(), endpoint);
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
