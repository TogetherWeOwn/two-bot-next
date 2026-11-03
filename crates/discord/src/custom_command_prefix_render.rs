//! Pure prefix render step for saved custom commands.
//!
//! The prefix boundary checks context BEFORE validating the stored template:
//! an enabled row that is both invalid and context-less refuses as
//! [`PrefixRenderRefusal::Context`] (audited `context_unavailable`), never as
//! `Render`. This module owns that ordering as a pure function so unit tests
//! pin it on every feature set; the DB-backed runtime maps the refusal into
//! its own error without reordering.

use two_bot_core::custom_commands::{render_template, validate_template, TemplateContext};
use two_bot_core::message_safety::has_message_text;

/// Prefix-execution refusal, listed in check order: context first, then template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrefixRenderRefusal {
    /// No guild name was available; the template was never inspected.
    Context,
    /// Definition-time validation or rendering rejected the stored template.
    Render,
}

/// Render a stored prefix template after the context check.
///
/// A `None` guild name refuses as [`PrefixRenderRefusal::Context`] without
/// touching the template; only a present context reaches definition-time
/// validation and rendering. The saved row is never mutated here. `args`
/// carries the remainder after the trigger token (empty when the message has
/// none); it substitutes literally in the same single pass, never re-scanned.
/// A render with no message text (for example an `{args}`-only template with
/// no remainder) refuses as [`PrefixRenderRefusal::Render`]: the boundary
/// never posts an empty payload, so the failure audits `render_failed`,
/// never `delivery_failed`.
pub(crate) fn render_prefix_response(
    guild_name: Option<&str>,
    template: &str,
    args: &str,
    user_mention: &str,
    username: &str,
    channel_mention: &str,
) -> Result<String, PrefixRenderRefusal> {
    let server = guild_name.ok_or(PrefixRenderRefusal::Context)?;
    validate_template(template).map_err(|_| PrefixRenderRefusal::Render)?;
    let rendered = render_template(
        template,
        &TemplateContext {
            user: user_mention.to_owned(),
            username: username.to_owned(),
            server: server.to_owned(),
            channel: channel_mention.to_owned(),
            args: args.to_owned(),
        },
    )
    .map_err(|_| PrefixRenderRefusal::Render)?;
    if !has_message_text(&rendered) {
        return Err(PrefixRenderRefusal::Render);
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVALID: &str = "saved-private-marker {user} {usre}";
    const VALID: &str = "Hi {user} {username} in {server} {channel}";

    #[test]
    fn invalid_template_without_context_refuses_as_context_first() {
        // Both guards trip; context wins. Swapping the two checks inside
        // `render_prefix_response` turns this red, which pins the order.
        assert_eq!(
            render_prefix_response(None, INVALID, "", "<@3333>", "tester", "<#4444>"),
            Err(PrefixRenderRefusal::Context)
        );
    }

    #[test]
    fn invalid_template_with_context_refuses_as_render() {
        assert_eq!(
            render_prefix_response(
                Some("Test guild"),
                INVALID,
                "",
                "<@3333>",
                "tester",
                "<#4444>"
            ),
            Err(PrefixRenderRefusal::Render)
        );
    }

    #[test]
    fn valid_template_without_context_refuses_as_context() {
        assert_eq!(
            render_prefix_response(None, VALID, "", "<@3333>", "tester", "<#4444>"),
            Err(PrefixRenderRefusal::Context)
        );
    }

    #[test]
    fn valid_template_with_context_renders_byte_identical() {
        assert_eq!(
            render_prefix_response(
                Some("Test guild"),
                VALID,
                "",
                "<@3333>",
                "tester",
                "<#4444>"
            ),
            Ok("Hi <@3333> tester in Test guild <#4444>".to_owned())
        );
    }

    #[test]
    fn args_only_template_with_no_text_refuses_as_render() {
        // An `{args}`-only template with absent, empty or whitespace-only
        // arguments renders no message text, so the boundary refuses instead
        // of posting an empty payload.
        for args in ["", "   ", "\u{a0}"] {
            assert_eq!(
                render_prefix_response(
                    Some("Test guild"),
                    "{args}",
                    args,
                    "<@3333>",
                    "tester",
                    "<#4444>"
                ),
                Err(PrefixRenderRefusal::Render)
            );
        }
        // Text around the placeholder still renders normally.
        assert_eq!(
            render_prefix_response(
                Some("Test guild"),
                "Says {args}",
                "",
                "<@3333>",
                "tester",
                "<#4444>"
            ),
            Ok("Says ".to_owned())
        );
    }

    #[test]
    fn args_substitute_literally_without_rescanning() {
        assert_eq!(
            render_prefix_response(
                Some("Test guild"),
                "Says {args}",
                "{user} @everyone",
                "<@3333>",
                "tester",
                "<#4444>"
            ),
            Ok("Says {user} @everyone".to_owned())
        );
    }
}
