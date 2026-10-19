//! Template-assistant select-and-retry build pipeline (spec V12,
//! `docs/voice-rooms.md`).
//!
//! Original implementation written from the behaviour spec only. This module
//! owns one build: from the admin's plain-language request to a validated
//! template with its six scenario previews and explanation, ready to show the
//! admin behind Apply/Refine/Cancel (that flow lives in a later slice).
//!
//! Pure orchestration: no HTTP client, endpoint configuration beyond the
//! already-gated [`AssistantConfig`], credentials, clock, database, monthly
//! cap or Discord wire types. The endpoint call is an injected
//! [`AssistantTransport`], so tests use a script and the real HTTPS wiring
//! (with its credential binding) arrives in a later slice without changing
//! this pipeline.
//!
//! ## Pipeline, in order
//!
//! 1. [`AssistantRequest::new`] bounds the four allowed inputs (request,
//!    guild templates, "no game" label, locale). Member names, presence and
//!    IDs have no field to travel in.
//! 2. [`ModelName::new`] checks the configured model. A blank model fails
//!    here, never by truncating to a different model.
//! 3. One deterministic chat-completions body is serialized; every attempt
//!    sends the same bytes.
//! 4. Up to [`MAX_BUILD_ATTEMPTS`] endpoint calls: each reply is parsed with
//!    [`parse_reply_each`], then suggestions are tried best-first with
//!    [`validate_template`]. The first suggestion that validates against all
//!    six scenarios wins with its previews and explanation. A suggestion the
//!    strict reply parse rejects never reaches scenario validation; it
//!    regenerates as the matching refusal instead (unknown token either way,
//!    unclosed delimiter as unbalanced syntax, blank as empty in every
//!    scenario).
//! 5. A refused suggestion set is regenerated — a fresh endpoint call —
//!    before the admin sees anything. When every attempt's suggestions are
//!    refused, the build fails with the last refusal.
//!
//! Transport and reply-shape failures fail the build immediately without
//! retrying: an identical body would get an identical rejection, and the
//! retry budget is for bad candidates, not a bad endpoint. Errors carry only
//! static text, counts and scenario labels — never reply text, template
//! sources, or member data.
//!
//! Residual parent work (later slices): the real HTTPS transport with the
//! endpoint credential binding, the per-guild monthly-cap ledger check before
//! calling, and the Apply/Refine/Cancel flow around the returned build.
//! Publication already runs through `InteractionRouter::publish_set` while
//! both voice gates are on.

use std::future::Future;

use crate::voice_assistant::AssistantConfig;
use crate::voice_assistant_request::{
    parse_reply_each, AssistantRequest, ModelName, ModelNameError, ReplyError, TemplateIssue,
};
use crate::voice_assistant_validate::{validate_template, TemplateRefusal};
use crate::voice_naming::ExtensionPolicy;
use crate::voice_template_lint::{Scenario, ScenarioRender};

/// Endpoint calls per build: the first attempt plus up to two regenerations
/// of refused suggestion sets. Bounded so a misbehaving endpoint cannot bill
/// an unbounded number of completions against one admin request.
pub const MAX_BUILD_ATTEMPTS: u8 = 3;

/// One successful build: the validated template source, the winning
/// suggestion's explanation, and the six scenario names in
/// [`crate::voice_template_lint::Scenario::ALL`] order — exactly what the
/// admin preview shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltTemplate {
    /// The accepted template source.
    pub template: String,
    /// The winning suggestion's explanation, trimmed.
    pub explanation: String,
    /// The six validated preview names; none used the fallback.
    pub previews: [ScenarioRender; 6],
}

/// The endpoint call behind [`build_template`]. The implementor POSTs `body`
/// — the exact chat-completions JSON — to `endpoint` (the full
/// chat-completions URL from configuration; no path is joined here) with a
/// JSON content type, and returns the raw response body for [`parse_reply_each`]
/// to bound and parse.
///
/// Implementors must bound the call with their own deadline and must redact
/// endpoint details from errors the way [`crate::backup::http::HttpError`]
/// wraps URLs and reasons in [`crate::Secret`]: build errors surface to logs,
/// and URLs can carry credentials. Any authorization header is the
/// transport's business; this pipeline never sees the credential.
pub trait AssistantTransport {
    /// Redacted transport failure.
    type Error;

    /// POST one chat-completions body; return the raw response body.
    fn post_chat_completions(
        &self,
        endpoint: &str,
        body: &str,
    ) -> impl Future<Output = Result<Vec<u8>, Self::Error>>;
}

/// Why one assistant build failed. No variant carries reply text, template
/// sources, or member data: refusals repeat only static reasons and scenario
/// labels, and reply errors repeat only field names, indexes and counts.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuildError<TransportError> {
    /// The admin's request text was blank after redaction and trimming.
    #[error("assistant request text is blank")]
    EmptyRequest,
    /// The configured model name was blank, over-long or had control
    /// characters.
    #[error("assistant model name is unusable: {0}")]
    BadModel(ModelNameError),
    /// The endpoint call itself failed (redacted by the transport).
    #[error("assistant endpoint call failed")]
    Transport(TransportError),
    /// The endpoint answered, but the reply was oversize, not JSON, an error
    /// object, wrongly shaped, or held no usable suggestion.
    #[error("assistant reply was unusable: {0}")]
    BadReply(ReplyError),
    /// Every attempt returned suggestions, but each suggestion failed the
    /// six-scenario validation. `refusal` is the last attempt's last
    /// refusal; the admin never saw any candidate.
    #[error("assistant produced no usable template after {attempts} attempts: {refusal}")]
    NoUsableSuggestion {
        /// Endpoint calls made, always [`MAX_BUILD_ATTEMPTS`].
        attempts: u8,
        /// The last refusal encountered.
        refusal: TemplateRefusal,
    },
}

/// Build one validated assistant template.
///
/// `config` must be a gated `Some` ([`AssistantConfig::from_map`] returns
/// `None` when disabled): a missing config means the assistant is off and
/// this function must not be called. `extensions` supplies the scenario
/// truth values for validation; `transport` makes the endpoint calls.
pub async fn build_template<T, E, Tr>(
    config: &AssistantConfig,
    request: &str,
    guild_templates: impl IntoIterator<Item = T>,
    no_game_label: &str,
    locale: &str,
    transport: &Tr,
    extensions: &E,
) -> Result<BuiltTemplate, BuildError<Tr::Error>>
where
    T: AsRef<str>,
    E: ExtensionPolicy,
    Tr: AssistantTransport,
{
    let assistant_request = AssistantRequest::new(request, guild_templates, no_game_label, locale)
        .map_err(|_| BuildError::EmptyRequest)?;
    let model = ModelName::new(config.model()).map_err(BuildError::BadModel)?;
    let body = assistant_request.chat_completions_json(&model);

    let mut refusal: Option<TemplateRefusal> = None;
    for _ in 0..MAX_BUILD_ATTEMPTS {
        let reply = transport
            .post_chat_completions(config.endpoint(), &body)
            .await
            .map_err(BuildError::Transport)?;
        // `parse_reply_each` guarantees a non-empty list, so a pass that
        // validates nothing always leaves a refusal behind.
        let suggestions = parse_reply_each(&reply).map_err(BuildError::BadReply)?;
        let mut validated = None;
        for suggestion in &suggestions {
            match suggestion {
                Ok(suggestion) => match validate_template(suggestion.template(), extensions) {
                    Ok(accepted) => {
                        validated = Some(BuiltTemplate {
                            template: suggestion.template().to_string(),
                            explanation: suggestion.explanation().to_string(),
                            previews: accepted.previews,
                        });
                        break;
                    }
                    Err(denied) => refusal = Some(denied),
                },
                // Byte hygiene the strict parse rejects never reaches scenario
                // validation; it regenerates as the matching refusal instead.
                Err(issue) => refusal = Some(strict_refusal(*issue)),
            }
        }
        if let Some(built) = validated {
            return Ok(built);
        }
    }
    Err(BuildError::NoUsableSuggestion {
        attempts: MAX_BUILD_ATTEMPTS,
        refusal: refusal.expect("parse_reply_each only returns non-empty suggestion lists"),
    })
}

/// The scenario refusal a strictly-rejected suggestion regenerates as. Blank
/// and unknown-token map exactly (a blank source is empty in every scenario;
/// tokens are always English). An unclosed delimiter or control character
/// would show raw in the name, so both map to unbalanced syntax. An
/// over-long source cannot be verified as a channel-name candidate, so it
/// maps to too-complex — refused rather than accepted unchecked.
fn strict_refusal(issue: TemplateIssue) -> TemplateRefusal {
    match issue {
        TemplateIssue::Blank => TemplateRefusal::EmptyName {
            scenarios: Scenario::ALL.to_vec(),
        },
        TemplateIssue::UnknownToken => TemplateRefusal::UnknownToken,
        TemplateIssue::UnbalancedDelimiter | TemplateIssue::ControlCharacter => {
            TemplateRefusal::UnbalancedSyntax
        }
        TemplateIssue::TooLong => TemplateRefusal::TooComplex,
    }
}
