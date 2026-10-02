//! Assistant-output scenario validation (spec V12, `docs/voice-rooms.md`).
//!
//! Pure core: one assistant-produced template is checked against the six
//! fixed [`Scenario`]s from [`crate::voice_template_lint`], and either
//! accepted with its six preview names or refused so the caller regenerates
//! it before an admin sees it. No network, database, Discord or endpoint
//! access. No member names, presence or IDs appear in any API: the scenarios
//! are fixed synthetic fixtures owned by the lint module, and refusals carry
//! only static text plus scenario labels.
//!
//! ## The three refusal classes (§V12)
//!
//! 1. **Empty names** ([`TemplateRefusal::EmptyName`]): the template renders
//!    empty — so the fallback name is used — in at least one scenario. A
//!    blank source fails here, empty in every scenario.
//! 2. **Never-matching conditions**
//!    ([`TemplateRefusal::NeverMatchingCondition`]): a condition is false in
//!    every scenario, so its yes-branch never shows. Truth comes from the
//!    caller's [`ExtensionPolicy`]; under passthrough no condition has a
//!    known truth value and none is reported.
//! 3. **Unknown tokens** ([`TemplateRefusal::UnknownToken`]): a `@@name@@`
//!    outside the engine's token list. Tokens are always English, so this is
//!    also the English-only check: a non-English token name is refused here.
//!    A name with non-ASCII letters never parses as a token at all and is
//!    refused as [`TemplateRefusal::UnbalancedSyntax`]. The explanation's
//!    language is caller-owned and never inspected — this function takes the
//!    template alone.
//!
//! Unclosed or stray delimiters are refused as
//! [`TemplateRefusal::UnbalancedSyntax`]: the name would show raw syntax to
//! members. A template the lint could not check completely (more blocks than
//! its scan bounds) is refused as [`TemplateRefusal::TooComplex`], never
//! accepted unchecked. Byte/shape hygiene (over-long, multi-line, control
//! characters) belongs to the V12b strict reply parser, which runs before
//! this module.
//!
//! When several problems exist at once, syntax wins, then unknown tokens,
//! then empty names, then conditions: `@@nope@@` reports the unknown token
//! rather than the empty renders it also causes.
//!
//! ```
//! use two_bot_core::voice_assistant_validate::validate_template;
//! use two_bot_core::voice_naming::PassthroughExtensions;
//!
//! let valid = validate_template("@@owner@@'s room ##", &PassthroughExtensions).unwrap();
//! assert_eq!(valid.previews.len(), 6);
//! assert!(validate_template("@@ownr@@'s room", &PassthroughExtensions).is_err());
//! ```

use crate::voice_naming::ExtensionPolicy;
use crate::voice_template_lint::{lint, preview, FindingKind, Scenario, ScenarioRender};

/// One accepted assistant template: the source plus the six scenario names
/// in [`Scenario::ALL`] order — exactly what the admin preview should show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedTemplate<'a> {
    /// The accepted template source, borrowed from the caller.
    pub template: &'a str,
    /// The six scenario names; none used the fallback.
    pub previews: [ScenarioRender; 6],
}

/// Why one assistant template was refused. The caller regenerates a refused
/// output before the admin sees it. Variants carry no template text: only
/// static reasons and scenario labels.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateRefusal {
    /// The template renders empty in at least one scenario, so the fallback
    /// name would be used there. `scenarios` lists every such scenario.
    #[error("template renders empty in at least one preview scenario; the fallback name is used")]
    EmptyName {
        /// Every scenario whose name fell back, in preview order.
        scenarios: Vec<Scenario>,
    },
    /// A condition is false in every preview scenario, so its yes-branch
    /// never shows.
    #[error("a condition is false in every preview scenario; its yes branch never shows")]
    NeverMatchingCondition,
    /// A `@@name@@` outside the engine's token list. Tokens are always
    /// English, so this is also the English-only check.
    #[error("template uses a token that does not exist; tokens are always English")]
    UnknownToken,
    /// An unclosed or stray delimiter; the name would show raw syntax.
    #[error("template has an unclosed or stray delimiter; it would render as literal text")]
    UnbalancedSyntax,
    /// The lint hit a scan bound before checking every block, so the
    /// template cannot be verified and is refused rather than accepted.
    #[error("template has more blocks than validation checks; it cannot be verified")]
    TooComplex,
}

/// Validate one assistant template against all six scenarios.
///
/// Runs the lint once and refuses on the highest-priority problem: syntax,
/// then tokens, then empty names, then conditions. A truncated lint report
/// with no finding is refused as [`TemplateRefusal::TooComplex`]. On success
/// the returned previews are the six rendered names to show the admin, so the
/// displayed preview is exactly the validated output.
pub fn validate_template<E: ExtensionPolicy>(
    template: &str,
    extensions: &E,
) -> Result<ValidatedTemplate<'_>, TemplateRefusal> {
    let report = lint(template, extensions);
    let mut refusal: Option<(u8, TemplateRefusal)> = None;
    let mut consider = |rank: u8, candidate: TemplateRefusal| {
        if refusal.as_ref().is_none_or(|(best, _)| rank < *best) {
            refusal = Some((rank, candidate));
        }
    };
    for finding in &report.findings {
        match &finding.kind {
            FindingKind::ParseError(_) => consider(0, TemplateRefusal::UnbalancedSyntax),
            FindingKind::UnknownToken => consider(1, TemplateRefusal::UnknownToken),
            FindingKind::EmptyRender { scenarios } => consider(
                2,
                TemplateRefusal::EmptyName {
                    scenarios: scenarios.clone(),
                },
            ),
            FindingKind::ConditionNeverMatches => {
                consider(3, TemplateRefusal::NeverMatchingCondition);
            }
        }
    }
    if let Some((_, refusal)) = refusal {
        return Err(refusal);
    }
    if report.truncated {
        return Err(TemplateRefusal::TooComplex);
    }
    Ok(ValidatedTemplate {
        template,
        previews: preview(template, extensions),
    })
}
